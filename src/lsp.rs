use {
    crate::{
        parse::Parse,
        query::{decls, default_module_name, loads, Decl, DeclKind, Query},
        to_range, zeek, File, FileId,
    },
    log::warn,
    std::{
        collections::HashSet,
        fmt::Debug,
        path::PathBuf,
        sync::{Arc, Mutex},
    },
    tower_lsp::{
        jsonrpc::{Error, ErrorCode, Result},
        lsp_types::{
            CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams,
            CompletionResponse, CreateFilesParams, DidChangeTextDocumentParams,
            DidOpenTextDocumentParams, DocumentSymbol, DocumentSymbolParams,
            DocumentSymbolResponse, Documentation, FileCreate, Hover, HoverContents, HoverParams,
            HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
            MarkedString, MessageType, OneOf, ServerCapabilities, SymbolKind,
            TextDocumentSyncCapability, TextDocumentSyncKind, Url,
        },
        Client, LanguageServer, LspService, Server,
    },
    tracing::instrument,
};

#[salsa::database(crate::parse::ParseStorage, crate::query::QueryStorage)]
#[derive(Default)]
pub struct Database {
    storage: salsa::Storage<Self>,
    files: HashSet<Arc<File>>,
    prefixes: HashSet<PathBuf>,
}

impl Database {
    #[must_use]
    pub fn get_file(&self, uri: &Url) -> Option<Arc<File>> {
        self.files.iter().find(|f| &f.id.0 == uri).map(Clone::clone)
    }
}

impl salsa::Database for Database {}

impl Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish()
    }
}

#[derive(Debug, Default)]
struct State {
    db: Database,
}

#[derive(Debug)]
struct Backend {
    client: Client,
    state: Mutex<State>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    #[instrument]
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::Full,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".into(), "?".into()]),
                    ..CompletionOptions::default()
                }),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    #[instrument]
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::Info, "server initialized!")
            .await;

        // Set up prefixes for normalization of system files.
        if let Ok(prefixes) = zeek::prefixes().await {
            if let Ok(mut state) = self.state.lock() {
                state.db.prefixes = prefixes;
            }
        }

        match zeek::system_files().await {
            Ok(files) => {
                self.did_create_files(CreateFilesParams {
                    files: files
                        .into_iter()
                        .filter_map(|f| {
                            Some(FileCreate {
                                uri: f.path.into_os_string().into_string().ok()?,
                            })
                        })
                        .collect(),
                })
                .await;
            }
            Err(e) => {
                self.client.log_message(MessageType::Error, e).await;
            }
        }
    }

    #[instrument]
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    #[instrument]
    async fn did_create_files(&self, params: CreateFilesParams) {
        let _process = params
            .files
            .iter()
            .filter_map(|f| {
                let uri = if let Ok(uri) = Url::from_file_path(&f.uri) {
                    uri
                } else {
                    warn!(
                        "ignoring {} since its path cannot be converted to an URI",
                        &f.uri
                    );
                    return None;
                };

                let source = match std::fs::read_to_string(&f.uri) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("failed to read '{}': {}", &f.uri, e);
                        return None;
                    }
                };

                let load = self
                    .load_pattern(&uri)
                    .expect("uri corresponds to a filename");

                if let Ok(state) = self.state.lock().as_deref_mut() {
                    let file = Arc::new(File {
                        id: uri.into(),
                        source,
                        load,
                    });

                    state.db.files.insert(file);
                };

                Some(())
            })
            .collect::<Vec<_>>();
    }

    #[instrument]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let source = params.text_document.text;
        let load = self
            .load_pattern(&uri)
            .expect("uri corresponds to a filename");

        if let Ok(state) = self.state.lock().as_deref_mut() {
            let file = Arc::new(File {
                id: uri.into(),
                source,
                load,
            });

            state.db.files.insert(file);
        }
    }

    #[instrument]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let changes = params.content_changes;
        assert_eq!(
            changes.len(),
            1,
            "more than one change received even though we only advertize full update mode"
        );
        let changes = changes.get(0).unwrap();
        assert!(changes.range.is_none(), "unexpected diff mode");

        let uri = params.text_document.uri;

        let load = self
            .load_pattern(&uri)
            .expect("uri corresponds to a filename");
        let id: FileId = uri.into();
        let source = changes.text.to_string();

        if let Ok(state) = self.state.lock().as_deref_mut() {
            state.db.files.insert(Arc::new(File { id, source, load }));
        }
    }

    #[instrument]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let params = &params.text_document_position_params;

        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(ErrorCode::InternalError))?;

        let file = match state.db.get_file(&params.text_document.uri) {
            Some(id) => id,
            None => {
                return Err(Error::new(ErrorCode::InvalidParams));
            }
        };

        // TODO(bbannier): This is more of a demo and debugging tool for now. Eventually this
        // should return some nice rendering of the hovered node.

        let tree = state.db.parse(file);
        let tree = match tree.as_ref() {
            Some(t) => t,
            None => return Ok(None),
        };

        let node = match tree.named_descendant_for_position(&params.position) {
            Some(n) => n,
            None => return Ok(None),
        };

        let hover = Hover {
            contents: HoverContents::Scalar(MarkedString::String(node.to_sexp())),
            range: to_range(node.range()).ok(),
        };

        Ok(Some(hover))
    }

    #[instrument]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(ErrorCode::InternalError))?;

        let file = match state.db.get_file(&params.text_document.uri) {
            Some(id) => id,
            None => return Ok(None),
        };

        let symbol = |d: &Decl| -> DocumentSymbol {
            #[allow(deprecated)]
            DocumentSymbol {
                name: d.id.clone(),
                range: d.range,
                selection_range: d.selection_range,
                kind: to_symbol_kind(d.kind),
                deprecated: None,
                detail: None,
                tags: None,
                children: None,
            }
        };

        let module = state
            .db
            .module(file)
            .ok_or_else(|| Error::new(ErrorCode::InternalError))?;

        Ok(Some(
            #[allow(deprecated)]
            DocumentSymbolResponse::Nested(vec![DocumentSymbol {
                name: module.id.clone().unwrap_or_else(|| {
                    default_module_name(&params.text_document.uri)
                        .unwrap_or("<invalid>")
                        .to_string()
                }),
                kind: SymbolKind::Module,
                range: module.range,
                selection_range: module.range,
                deprecated: None,

                detail: None,
                tags: None,
                children: Some(module.decls.iter().map(symbol).collect()),
            }]),
        ))
    }

    #[instrument]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position;

        let (file, tree) = {
            let state = self
                .state
                .lock()
                .map_err(|_| Error::new(ErrorCode::InternalError))?;

            let file = match state.db.get_file(&position.text_document.uri) {
                Some(id) => id,
                None => return Ok(None),
            };

            let tree = match state.db.parse(file.clone()) {
                Some(t) => t,
                None => return Ok(None),
            };

            (file, tree)
        };

        let node = match tree.descendant_for_position(&position.position) {
            Some(n) => n,
            None => return Ok(None),
        };

        let to_completion_item = |d: Decl| CompletionItem {
            label: d.id,
            kind: Some(to_completion_item_kind(d.kind)),
            documentation: Some(Documentation::String(d.documentation)),
            ..CompletionItem::default()
        };

        let mut items: Vec<_> = {
            let mut items = Vec::new();
            let mut node = node;
            loop {
                items.append(&mut decls(node, &file.source));
                node = match node.parent() {
                    Some(n) => n,
                    None => break,
                };
            }
            items.into_iter().map(to_completion_item).collect()
        };

        // Add an decls found in implicitly or explicitly loaded modules.
        items.extend(
            self.external_decls(&file)?
                .into_iter()
                .map(to_completion_item),
        );

        Ok(Some(CompletionResponse::from(items)))
    }
}

impl Backend {
    // TODO(bbannier): move this into query.rs and cache it.
    fn external_decls(&self, file: &Arc<File>) -> Result<Vec<Decl>> {
        // TODO(bbannier): Refactor this pattern into a helper lock: Self -> Result<State>.
        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(ErrorCode::InternalError))?;

        let tree = state.db.parse(file.clone());
        let tree = match tree.as_ref() {
            Some(t) => t,
            None => return Ok(Vec::new()),
        };

        // Get loaded modules for this file.
        let loads = loads(tree.root_node(), &file.source)
            .into_iter()
            .map(String::from)
            .collect::<HashSet<_>>();

        // The list of pulled in files.
        let mut files = state
            .db
            .files
            .iter()
            .filter(|f| {
                // TODO(bbannier): Report unloadable modules.
                loads.iter().any(|l| &f.load == l)
            })
            .cloned()
            .collect::<HashSet<_>>();

        // Recursively resolve all pulled in files.
        loop {
            let mut new_files = HashSet::new();

            for file in &files {
                let module = match state.db.module(file.clone()) {
                    Some(m) => m,
                    None => continue,
                };

                for load in &module.loads {
                    if let Some(file) = state.db.files.iter().find(|f| &f.load == load) {
                        if files.contains(file) {
                            // Already known.
                            continue;
                        }

                        new_files.insert(file.clone());
                    } else {
                        // TODO(bbannier): report unresolvable loads.
                    }
                }
            }

            if new_files.is_empty() {
                break;
            }

            for f in new_files {
                files.insert(f);
            }
        }

        let modules = files.into_iter().filter_map(|file| state.db.module(file));

        Ok(modules
            .filter_map(|module| {
                let module_id = match &module.id {
                    Some(id) => id,
                    None => default_module_name(&file.id)?,
                };

                Some(
                    module
                        .decls
                        .clone()
                        .into_iter()
                        .map(|mut d| {
                            d.id = format!("{m}::{d}", m = module_id, d = d.id);
                            d
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect())
    }

    /// The pattern under which the give uri can be loaded.
    fn load_pattern(&self, uri: &Url) -> Option<String> {
        let file = uri.to_file_path().expect("uri should be a valid path");

        if let Ok(state) = self.state.lock() {
            if let Some(file) = state.db.get_file(uri) {
                // File is known.
                return Some(file.load.clone());
            }

            if let Some(from_prefix) = state
                .db
                .prefixes
                .iter()
                .find_map(|p| file.strip_prefix(p).ok())
                .map(|p| {
                    if p.ends_with("__load__.zeek") || p.ends_with("__preload__.zeek") {
                        p.to_path_buf()
                    } else {
                        p.with_extension("")
                    }
                })
            {
                // File is from a known prefix.
                return Some(from_prefix.as_os_str().to_string_lossy().into());
            }
        }

        // TODO(bbannier): take the workspace (explicit from initialization or implicit from
        // presence of `__(pre)load__.zeek` files) into account.
        file.file_stem()
            .map(|s| format!("./{}", s.to_string_lossy()))
        // TODO(bbannier): report uris without file stem?
    }
}

fn to_symbol_kind(kind: DeclKind) -> SymbolKind {
    match kind {
        DeclKind::Global | DeclKind::Variable | DeclKind::Redef => SymbolKind::Variable,
        DeclKind::Option => SymbolKind::Property,
        DeclKind::Const => SymbolKind::Constant,
        DeclKind::RedefEnum => SymbolKind::Enum,
        DeclKind::RedefRecord => SymbolKind::Interface,
        DeclKind::Type => SymbolKind::Class,
        DeclKind::Func => SymbolKind::Function,
        DeclKind::Hook => SymbolKind::Operator,
        DeclKind::Event => SymbolKind::Event,
    }
}

fn to_completion_item_kind(kind: DeclKind) -> CompletionItemKind {
    match kind {
        DeclKind::Global | DeclKind::Variable | DeclKind::Redef => CompletionItemKind::Variable,
        DeclKind::Option => CompletionItemKind::Property,
        DeclKind::Const => CompletionItemKind::Constant,
        DeclKind::RedefEnum => CompletionItemKind::Enum,
        DeclKind::RedefRecord => CompletionItemKind::Interface,
        DeclKind::Type => CompletionItemKind::Class,
        DeclKind::Func => CompletionItemKind::Function,
        DeclKind::Hook => CompletionItemKind::Operator,
        DeclKind::Event => CompletionItemKind::Event,
    }
}

pub async fn run() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) = LspService::new(|client| Backend {
        client,
        state: Mutex::default(),
    });
    Server::new(stdin, stdout)
        .interleave(messages)
        .serve(service)
        .await;
}
