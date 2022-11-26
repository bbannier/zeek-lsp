use std::{collections::BTreeSet, sync::Arc};

use crate::{
    ast::{self, Ast},
    lsp::Database,
    parse::Parse,
    query::{self, Decl, DeclKind, NodeLocation, Query},
    zeek, Files,
};
use eyre::Result;
use itertools::Itertools;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, Documentation,
    MarkupContent, Position,
};

#[allow(clippy::too_many_lines)]
pub(crate) fn complete(
    state: &Database,
    params: CompletionParams,
) -> Result<Option<CompletionResponse>> {
    let uri = Arc::new(params.text_document_position.text_document.uri);
    let position = params.text_document_position.position;

    let source = state.source(uri.clone());

    let tree = match state.parse(uri.clone()) {
        Some(t) => t,
        None => return Ok(None),
    };

    // Get the node directly under the cursor as a starting point.
    let root = tree.root_node();
    let mut node = match root.descendant_for_position(position) {
        Some(n) => n,
        None => return Ok(None),
    };

    // If the node has no interesting text try to find an earlier node with text.
    while node
        .utf8_text(source.as_bytes())
        .ok()
        // The grammar might expose newlines as AST nodes. Such nodes should be ignored for completion.
        .map(str::trim)
        // The grammar might expose `$` or `?$` in a node. Strip it away. This also takes care of
        // explicit nodes for just the field access or check.
        .map(|s| s.replace(['$', '?'], ""))
        .map_or(0, |s| s.len())
        == 0
    {
        // If we are completing at the end of a line the end of the node will be on the next
        // line. Instead search the next node _before the_start_ of the current node.
        let start = node.range().start.character;
        if start == 0 {
            break;
        }

        node = match root.descendant_for_position(Position {
            character: start - 1,
            ..position
        }) {
            Some(n) => n,
            None => break,
        };
    }

    let text_at_completion = node
        .utf8_text(source.as_bytes())?
        // This shouldn't happen; if we cannot get the node text there is some UTF-8 error.
        .lines()
        .next()
        .map(str::trim);

    // If we are completing after `$` try to return all fields for client-side filtering.
    // TODO(bbannier): we should also handle `$` in record initializations.
    if params
        .context
        .and_then(|ctx| ctx.trigger_character)
        .map_or(false, |c| c == "$")
        || root
            .descendant_for_position(Position::new(
                node.range().end.line,
                node.range().end.character,
            ))
            .and_then(|next_node| next_node.utf8_text(source.as_bytes()).ok())
            .map_or(false, |text| text.ends_with('$'))
        || node.parent().map_or(false, |p| {
            p.kind() == "field_access" || p.kind() == "field_check"
        })
    {
        // If we are completing with something after the `$` (e.g., `foo$a`), instead
        // obtain the stem (`foo`) for resolving and then filter any possible fields with
        // the given text (`a`).
        let stem = node
            .parent()
            .filter(|p| p.kind() == "field_access" || p.kind() == "field_check")
            .and_then(|p| p.named_child("expr"));
        let preselection = stem.and_then(|_| node.utf8_text(source.as_bytes()).ok());

        // If we have a stem, perform any resolving with it; else use the original node.
        let node = stem.unwrap_or(node);

        if let Some(r) = state.resolve(NodeLocation::from_node(uri.clone(), node)) {
            let decl = state.typ(r);

            // Compute completion.
            if let Some(decl) = decl {
                if let DeclKind::Type(fields) = &decl.kind {
                    return Ok(Some(CompletionResponse::from(
                        fields
                            .iter()
                            .filter(|decl| {
                                // If we have a preselection, narrow down fields to report.
                                preselection.map_or(true, |pre| decl.id.starts_with(pre))
                            })
                            .map(to_completion_item)
                            .filter_map(|item| {
                                // By default we use FQIDs for completion labels. Since for
                                // record fields this would be e.g., `mod::rec::field` where we
                                // want just `field`, rework them slightly.
                                let label = item.label.split("::").last()?.to_string();
                                Some(CompletionItem { label, ..item })
                            })
                            .collect::<Vec<_>>(),
                    )));
                }
            }
        }
    }

    // If we are completing a file return valid load patterns.
    if node.kind() == "file" {
        return Ok(Some(CompletionResponse::from(
            state
                .possible_loads(uri)
                .iter()
                .map(|load| CompletionItem {
                    label: load.clone(),
                    kind: Some(CompletionItemKind::FILE),
                    ..CompletionItem::default()
                })
                .collect::<Vec<_>>(),
        )));
    }

    // If we are completing a function/event/hook definition complete from declarations.
    if node.kind() == "id" {
        if let Some(kind) = source
            .lines()
            .nth(usize::try_from(node.range().start.line).expect("too many lines"))
            .and_then(|line| {
                let re = regex::Regex::new(r"^(\w+)\s+\w*").expect("invalid regexp");
                Some(re.captures(line)?.get(1)?.as_str())
            })
        {
            return Ok(Some(CompletionResponse::from(
                state
                    .decls(uri.clone())
                    .iter()
                    .chain(state.implicit_decls().iter())
                    .chain(state.explicit_decls_recursive(uri).iter())
                    .filter(|d| match &d.kind {
                        DeclKind::EventDecl(_) => kind == "event",
                        DeclKind::FuncDecl(_) => kind == "function",
                        DeclKind::HookDecl(_) => kind == "hook",
                        _ => false,
                    })
                    .unique()
                    .filter_map(|d| {
                        let item = to_completion_item(d);
                        let signature = match &d.kind {
                            DeclKind::EventDecl(s)
                            | DeclKind::FuncDecl(s)
                            | DeclKind::HookDecl(s) => {
                                let args = &s.args;
                                Some(
                                    args.iter()
                                        .filter_map(|d| {
                                            let tree = state.parse(d.uri.clone())?;
                                            let source = state.source(d.uri.clone());
                                            tree.root_node()
                                                .named_descendant_for_point_range(
                                                    d.selection_range,
                                                )?
                                                .utf8_text(source.as_bytes())
                                                .map(String::from)
                                                .ok()
                                        })
                                        .join(", "),
                                )
                            }
                            _ => None,
                        }?;

                        Some(CompletionItem {
                            label: format!("{id}({signature}) {{}}", id = item.label),
                            ..item
                        })
                    })
                    .collect::<Vec<_>>(),
            )));
        }
    }

    // We are just completing some arbitrary identifier at this point.
    let mut items = BTreeSet::new();
    let mut node = node;

    let current_module = root
        .named_child("module_decl")
        .and_then(|m| m.named_child("id"))
        .and_then(|id| id.utf8_text(source.as_bytes()).ok());

    loop {
        for d in query::decls_(node, uri.clone(), source.as_bytes()) {
            // Slightly fudge the ID we use for local declarations by removing the current
            // module from the FQID.
            let fqid = match current_module {
                Some(mid) => {
                    let id = d.fqid.as_str();
                    id.strip_prefix(&format!("{mid}::")).unwrap_or(id)
                }
                None => &d.fqid,
            }
            .into();
            items.insert(Decl { fqid, ..d });
        }

        node = match node.parent() {
            Some(n) => n,
            None => break,
        };
    }

    let loaded_decls = state.explicit_decls_recursive(uri);
    let implicit_decls = state.implicit_decls();

    let other_decls = loaded_decls
            .iter()
            .chain(implicit_decls.iter())
            .filter(|i| {
                // Filter out redefs since they only add noise.
                !ast::is_redef(i) &&
                    // Only return external decls which somehow match the text to complete to keep the response sent to the client small.
                    if let Some(text) = text_at_completion {
                        rust_fuzzy_search::fuzzy_compare(&text.to_lowercase(), &i.fqid.to_lowercase())
                            > 0.0
                    } else {
                        true
                    }
            });

    Ok(Some(CompletionResponse::from(
        items
            .iter()
            .chain(other_decls)
            .unique()
            .map(to_completion_item)
            // Also send filtered down keywords to the client.
            .chain(zeek::KEYWORDS.iter().filter_map(|kw| {
                let should_include = if let Some(text) = text_at_completion {
                    text.is_empty()
                        || rust_fuzzy_search::fuzzy_compare(
                            &text.to_lowercase(),
                            &kw.to_lowercase(),
                        ) > 0.0
                } else {
                    true
                };

                if should_include {
                    Some(CompletionItem {
                        kind: Some(CompletionItemKind::KEYWORD),
                        label: (*kw).to_string(),
                        ..CompletionItem::default()
                    })
                } else {
                    None
                }
            }))
            .collect::<Vec<_>>(),
    )))
}

fn to_completion_item(d: &Decl) -> CompletionItem {
    CompletionItem {
        label: d.fqid.clone(),
        kind: Some(to_completion_item_kind(&d.kind)),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: tower_lsp::lsp_types::MarkupKind::Markdown,
            value: d.documentation.clone(),
        })),
        ..CompletionItem::default()
    }
}

fn to_completion_item_kind(kind: &DeclKind) -> CompletionItemKind {
    match kind {
        DeclKind::Global | DeclKind::Variable | DeclKind::Redef | DeclKind::LoopIndex(_, _) => {
            CompletionItemKind::VARIABLE
        }
        DeclKind::Option => CompletionItemKind::PROPERTY,
        DeclKind::Const => CompletionItemKind::CONSTANT,
        DeclKind::Enum(_) | DeclKind::RedefEnum(_) => CompletionItemKind::ENUM,
        DeclKind::Type(_) | DeclKind::RedefRecord(_) => CompletionItemKind::CLASS,
        DeclKind::FuncDecl(_) | DeclKind::FuncDef(_) => CompletionItemKind::FUNCTION,
        DeclKind::HookDecl(_) | DeclKind::HookDef(_) => CompletionItemKind::OPERATOR,
        DeclKind::EventDecl(_) | DeclKind::EventDef(_) => CompletionItemKind::EVENT,
        DeclKind::Field => CompletionItemKind::FIELD,
        DeclKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use insta::assert_debug_snapshot;
    use tower_lsp::{
        lsp_types::{
            CompletionContext, CompletionParams, CompletionResponse, CompletionTriggerKind,
            PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, Url,
            WorkDoneProgressParams,
        },
        LanguageServer,
    };

    use crate::lsp::test::{serve, TestDatabase};

    #[tokio::test]
    async fn field_access() {
        let mut db = TestDatabase::new();

        let uri1 = Arc::new(Url::from_file_path("/x.zeek").unwrap());
        db.add_file(
            uri1.clone(),
            "type X: record { abc: count; };
            global foo: X;
            foo$
            ",
        );

        let uri2 = Arc::new(Url::from_file_path("/y.zeek").unwrap());
        db.add_file(
            uri2.clone(),
            "type X: record { abc: count; };
            global foo: X;
            foo?$
            ",
        );

        let server = serve(db);

        let uri = uri1;
        {
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                    position: Position::new(2, 16),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            };

            assert_debug_snapshot!(
                server
                    .completion(CompletionParams {
                        context: None,
                        ..params.clone()
                    })
                    .await
            );

            assert_debug_snapshot!(
                server
                    .completion(CompletionParams {
                        context: Some(CompletionContext {
                            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                            trigger_character: Some("$".into()),
                        },),
                        ..params
                    })
                    .await
            );
        }

        let uri = uri2;
        {
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                    position: Position::new(2, 17),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            };

            assert_debug_snapshot!(
                server
                    .completion(CompletionParams {
                        context: None,
                        ..params.clone()
                    })
                    .await
            );

            assert_debug_snapshot!(
                server
                    .completion(CompletionParams {
                        context: Some(CompletionContext {
                            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
                            trigger_character: Some("$".into()),
                        },),
                        ..params
                    })
                    .await
            );
        }
    }

    #[tokio::test]
    async fn field_access_partial() {
        let mut db = TestDatabase::new();

        let uri1 = Arc::new(Url::from_file_path("/x.zeek").unwrap());
        db.add_file(
            uri1.clone(),
            "type X: record { abc: count; };
            global foo: X;
            foo$a
            ",
        );

        let uri2 = Arc::new(Url::from_file_path("/x.zeek").unwrap());
        db.add_file(
            uri2.clone(),
            "type X: record { abc: count; };
            global foo: X;
            foo?$a
            ",
        );

        let server = serve(db);

        {
            let uri = uri1.clone();
            let position = Position::new(2, 17);
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            };
            assert_debug_snapshot!(
                server
                    .completion(CompletionParams {
                        context: None,
                        ..params.clone()
                    })
                    .await
            );
        }

        {
            let uri = uri2.clone();
            let position = Position::new(2, 17);
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            };
            assert_debug_snapshot!(
                server
                    .completion(CompletionParams {
                        context: None,
                        ..params.clone()
                    })
                    .await
            );
        }
    }

    #[tokio::test]
    async fn load() {
        let mut db = TestDatabase::new();
        db.add_prefix("/p1");
        db.add_prefix("/p2");
        db.add_file(
            Arc::new(Url::from_file_path("/p1/foo/a1.zeek").unwrap()),
            "",
        );
        db.add_file(
            Arc::new(Url::from_file_path("/p2/foo/b1.zeek").unwrap()),
            "",
        );

        let uri = Arc::new(Url::from_file_path("/x/x.zeek").unwrap());
        db.add_file(uri.clone(), "@load f");

        let server = serve(db);

        assert_debug_snapshot!(
            server
                .completion(CompletionParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                        position: Position::new(0, 6),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .await
        );
    }

    #[tokio::test]
    async fn event() {
        let mut db = TestDatabase::new();
        let uri = Arc::new(Url::from_file_path("/x.zeek").unwrap());
        db.add_file(
            uri.clone(),
            "
export {
    global evt: event(c: count, s: string);
    global fct: function(c: count, s: string);
    global hok: hook(c: count, s: string);
}

event e
function f
hook h
",
        );

        let server = serve(db);

        assert_debug_snapshot!(
            server
                .completion(CompletionParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                        position: Position::new(7, 6),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .await
        );

        assert_debug_snapshot!(
            server
                .completion(CompletionParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                        position: Position::new(8, 10),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .await
        );

        assert_debug_snapshot!(
            server
                .completion(CompletionParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                        position: Position::new(9, 6),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .await
        );
    }

    #[tokio::test]
    async fn keyword() {
        let mut db = TestDatabase::new();
        let uri = Arc::new(Url::from_file_path("/x.zeek").unwrap());
        db.add_file(
            uri.clone(),
            "
function foo() {}
f",
        );

        let server = serve(db);

        let result = server
            .completion(CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier::new(uri.as_ref().clone()),
                    position: Position::new(2, 0),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            })
            .await;

        // Sort results for debug output diffing.
        let result = match result {
            Ok(Some(CompletionResponse::Array(mut r))) => {
                r.sort_by(|a, b| a.label.cmp(&b.label));
                r
            }
            _ => panic!(),
        };

        assert_debug_snapshot!(result);
    }
}
