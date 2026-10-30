//! `/mcp`: the Model Context Protocol over Streamable HTTP. Each tool calls one RPC method
//! with its arguments as the params, as an agent acting for the token's user, so the
//! journal tells the agent's changes from the user's own.

use std::sync::Arc;

use asteria_framework::UserId;
use asteria_framework::journal::Actor;
use asteria_framework_api::ItemId;
use asteria_framework_api::{
    FOLDER_LIST, LIBRARY_INFO, LIBRARY_RESCAN, MAX_LABEL_CHARS, MEDIA_GET, MEDIA_UPDATE,
    ORGANIZE_APPLY, ORGANIZE_HISTORY, ORGANIZE_STATUS, ORGANIZE_UNDO, SERVER_INFO, UPLOAD_ABORT,
    UPLOAD_BEGIN, UPLOAD_CHECK, UPLOAD_COMMIT, UPLOAD_STATUS,
};
use axum::http::request::Parts;
use base64::Engine as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};

use crate::ext::{AppExtension, McpTool};
use crate::media_http::{ThumbError, thumb_file};
use crate::process::Preset;
use crate::rpc;
use crate::state::AppState;

/// What the model is told about the server when it connects.
const INSTRUCTIONS: &str = "Asteria manages a self-hosted media library. Browse with \
folder_list and media_get; look at pictures with media_preview. Edit ratings, tags and \
descriptions with media_update. Move, rename and trash files with organize_apply: run it with \
dry_run: true first and show the user the result before a large change, and give every batch \
a label that says what it does. If organize_apply or organize_undo answers running: true, poll \
organize_status with the batch_id. Every batch can be undone with organize_undo; nothing is \
ever deleted for good.";

/// An object schema with `properties`, of which `required` must be present.
fn object(properties: Value, required: &[&str]) -> Map<String, Value> {
    let Value::Object(schema) = json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    }) else {
        unreachable!("json! of an object literal is an object")
    };
    schema
}

fn tool(name: &str, method: &str, description: &str, input_schema: Map<String, Value>) -> McpTool {
    McpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
        method: method.into(),
    }
}

/// The tools for the framework's RPC methods. The schemas follow the params types in
/// `asteria-framework-api`.
pub(crate) fn framework_tools() -> Vec<McpTool> {
    let id = json!({"type": "string", "description": "The item's id."});
    let batch_id = json!({"type": "string", "description": "The batch's id."});
    let cursor = json!({"type": "string", "description": "next_cursor of the previous page."});
    let dry_run = json!({
        "type": "boolean",
        "description": "Report what would happen without changing anything.",
    });
    vec![
        tool(
            "library_info",
            LIBRARY_INFO,
            "Describes the library: its id, owner, number of items, whether a scan is \
             running, and a revision that changes whenever the library does.",
            object(json!({}), &[]),
        ),
        tool(
            "folder_list",
            FOLDER_LIST,
            "Lists one folder of the library: the names of its subfolders and its media \
             items (id, path, kind, size, rating, subjects, description, label). Pass \
             next_cursor back as cursor for the next page.",
            object(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Library-relative folder with / separators; \"\" is the root.",
                    },
                    "cursor": cursor,
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000},
                }),
                &["path"],
            ),
        ),
        tool(
            "media_get",
            MEDIA_GET,
            "Returns one media item by id: its path, kind, size, modification time, your \
             rating, subjects, hierarchical subjects, description and label.",
            object(json!({"id": id}), &["id"]),
        ),
        tool(
            "media_update",
            MEDIA_UPDATE,
            "Changes an item's rating, subjects, hierarchical subjects, description or label \
             and returns the updated item. Fields left out stay as they are; a list replaces \
             the whole list, and null clears the description or label.",
            object(
                json!({
                    "id": id,
                    "rating": {
                        "type": "integer",
                        "minimum": -1,
                        "maximum": 5,
                        "description": "-1 rejected, 0 unrated, 1 to 5 stars.",
                    },
                    "subjects": {"type": "array", "items": {"type": "string"}},
                    "hierarchical_subjects": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Levels separated by |, as in \"animal|cat\".",
                    },
                    "description": {"type": ["string", "null"]},
                    "label": {"type": ["string", "null"]},
                }),
                &["id"],
            ),
        ),
        tool(
            "organize_apply",
            ORGANIZE_APPLY,
            "Moves, renames and trashes items as one batch and returns its batch_id with a \
             result per operation (done, or skipped with the reason). Before a large change, \
             run it with dry_run: true and show the user the result. If the answer has \
             running: true the batch goes on in the background; wait for it with \
             organize_status.",
            object(
                json!({
                    "ops": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "item_id": id,
                                "kind": {"type": "string", "enum": ["move", "trash"]},
                                "to": {
                                    "type": "string",
                                    "description": "For a move: the new library-relative path with / separators.",
                                },
                                "file_hash": {
                                    "type": "string",
                                    "description": "The item's file_hash as you saw it; the operation is skipped if the file changed since.",
                                },
                            },
                            "required": ["item_id", "kind"],
                            "additionalProperties": false,
                        },
                    },
                    "label": {
                        "type": "string",
                        "maxLength": MAX_LABEL_CHARS,
                        "description": "What the batch does, shown in the history.",
                    },
                    "dry_run": dry_run,
                }),
                &["ops"],
            ),
        ),
        tool(
            "organize_status",
            ORGANIZE_STATUS,
            "Reports a batch of organize_apply or organize_undo: whether it is still \
             running, whether it completed, and how many operations were done or skipped, \
             with the per-operation results while the server still has them.",
            object(json!({"batch_id": batch_id}), &["batch_id"]),
        ),
        tool(
            "organize_undo",
            ORGANIZE_UNDO,
            "Undoes a batch by moving its files back, as a new batch, and answers like \
             organize_apply (running: true means wait with organize_status). The undo is a \
             batch too, so undoing it redoes the change.",
            object(
                json!({"batch_id": batch_id, "dry_run": dry_run}),
                &["batch_id"],
            ),
        ),
        tool(
            "organize_history",
            ORGANIZE_HISTORY,
            "Lists past batches, newest first: batch_id, user, whether an agent acted for \
             them, time, label, counts, and what undid or was undone. Pass next_cursor back \
             as cursor for the next page.",
            object(
                json!({
                    "limit": {"type": "integer", "minimum": 1},
                    "cursor": cursor,
                }),
                &[],
            ),
        ),
        tool(
            MEDIA_PREVIEW,
            "",
            "Shows thumbnails of up to 4 items as images, so you can see what a photo or \
             video shows, or an album's cover.",
            object(
                json!({
                    "ids": {
                        "type": "array",
                        "items": id,
                        "minItems": 1,
                        "maxItems": MAX_PREVIEW_ITEMS,
                    },
                }),
                &["ids"],
            ),
        ),
    ]
}

/// The tool that shows thumbnails. It has no RPC method: `call_tool` answers it itself.
const MEDIA_PREVIEW: &str = "media_preview";

/// The most items one `media_preview` shows.
const MAX_PREVIEW_ITEMS: usize = 4;

/// Every method the framework answers. App tools may not call these: the framework decides
/// which of its methods an agent gets (uploads and rescans it does not).
const FRAMEWORK_METHODS: [&str; 15] = [
    SERVER_INFO,
    LIBRARY_INFO,
    LIBRARY_RESCAN,
    FOLDER_LIST,
    MEDIA_GET,
    MEDIA_UPDATE,
    UPLOAD_CHECK,
    UPLOAD_BEGIN,
    UPLOAD_STATUS,
    UPLOAD_COMMIT,
    UPLOAD_ABORT,
    ORGANIZE_APPLY,
    ORGANIZE_UNDO,
    ORGANIZE_HISTORY,
    ORGANIZE_STATUS,
];

/// The framework's tools and then the app's. An app tool named like a framework one, or
/// calling a framework method, is dropped.
pub(crate) fn tool_table(ext: &dyn AppExtension) -> Vec<McpTool> {
    let mut tools = framework_tools();
    // `media_preview` is in the list, so an app tool of that name is dropped as a shadow.
    for t in ext.mcp_tools() {
        if tools.iter().any(|f| f.name == t.name) {
            tracing::warn!(tool = %t.name, "app MCP tool shadows a framework tool; dropped");
        } else if FRAMEWORK_METHODS.contains(&t.method.as_str()) {
            tracing::warn!(
                tool = %t.name,
                method = %t.method,
                "app MCP tool calls a framework method; dropped"
            );
        } else {
            tools.push(t);
        }
    }
    tools
}

/// The MCP server behind `/mcp`.
#[derive(Clone)]
pub(crate) struct AsteriaMcp {
    state: AppState,
    tools: Arc<Vec<McpTool>>,
}

impl AsteriaMcp {
    pub(crate) fn new(state: AppState) -> AsteriaMcp {
        let tools = Arc::new(tool_table(state.extension().as_ref()));
        AsteriaMcp { state, tools }
    }
}

impl AsteriaMcp {
    /// A caption and a thumbnail per item; an item that fails gets only a line saying why,
    /// and that is not a tool error.
    async fn media_preview(&self, user: UserId, args: Value) -> CallToolResult {
        let ids: Vec<String> = match args.get("ids").map(|v| serde_json::from_value(v.clone())) {
            Some(Ok(ids)) => ids,
            _ => {
                return CallToolResult::error(vec![ContentBlock::text(
                    "ids must be a list of strings",
                )]);
            }
        };
        if ids.is_empty() {
            return CallToolResult::error(vec![ContentBlock::text("at least one item")]);
        }
        if ids.len() > MAX_PREVIEW_ITEMS {
            return CallToolResult::error(vec![ContentBlock::text("at most 4 items per preview")]);
        }
        let mut content = Vec::new();
        for id in ids {
            match self.thumbnail_data(user, &id).await {
                Ok(data) => {
                    content.push(ContentBlock::text(format!("{id}:")));
                    content.push(ContentBlock::image(data, Preset::Thumb.mime()));
                }
                Err(e) => {
                    let why = match e {
                        ThumbError::NotFound => "not found",
                        ThumbError::NotReady => "not ready yet, try again shortly",
                        ThumbError::Unprocessable => "cannot be rendered",
                        ThumbError::Internal => "failed",
                    };
                    content.push(ContentBlock::text(format!("{id}: {why}")));
                }
            }
        }
        CallToolResult::success(content)
    }

    async fn thumbnail_data(&self, user: UserId, id: &str) -> Result<String, ThumbError> {
        let path = thumb_file(&self.state, user, ItemId(id.to_owned()), Preset::Thumb).await?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            tracing::error!("reading a thumbnail failed: {e}");
            ThumbError::Internal
        })?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

/// The user the auth layer let in; rmcp hands the request's HTTP parts to the handler.
fn caller(ctx: &RequestContext<RoleServer>) -> Result<UserId, ErrorData> {
    ctx.extensions
        .get::<Parts>()
        .and_then(|p| p.extensions.get::<UserId>())
        .copied()
        .ok_or_else(|| ErrorData::internal_error("no authenticated user", None))
}

impl ServerHandler for AsteriaMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS);
        info.server_info = Implementation::new(self.state.app(), env!("CARGO_PKG_VERSION"));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.tools.iter().map(|t| {
            Tool::new(
                t.name.clone(),
                t.description.clone(),
                Arc::new(t.input_schema.clone()),
            )
        });
        Ok(ListToolsResult::with_all_items(tools.collect()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name == MEDIA_PREVIEW {
            let user = caller(&ctx)?;
            let args = Value::Object(request.arguments.unwrap_or_default());
            return Ok(self.media_preview(user, args).await.into());
        }
        let Some(tool) = self.tools.iter().find(|t| t.name == request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        };
        let user = caller(&ctx)?;
        let params = Value::Object(request.arguments.unwrap_or_default());
        let actor = Actor::Agent { on_behalf_of: user };
        let result = match rpc::call(&self.state, actor, &tool.method, params).await {
            Ok(v) => {
                let text = serde_json::to_string_pretty(&v)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.message)]),
        };
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use asteria_framework::MediaKind;
    use asteria_framework_api::{
        FolderListParams, MediaGetParams, MediaUpdateParams, OrganizeApplyParams,
        OrganizeHistoryParams, OrganizeStatusParams, OrganizeUndoParams,
    };
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;
    use crate::ext::{AppExtension, McpTool};

    fn parses<P: DeserializeOwned>(v: &Value) {
        serde_json::from_value::<P>(v.clone()).unwrap();
    }

    #[test]
    fn every_framework_tool_names_a_method() {
        let tools = framework_tools();
        assert_eq!(tools.len(), 9);
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 9);
        for t in tools.iter().filter(|t| t.name != MEDIA_PREVIEW) {
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            assert!(!t.method.is_empty(), "{}", t.name);
            assert!(!t.description.is_empty(), "{}", t.name);
        }
    }

    /// Checks that an example parses as the tool's params.
    type Check = fn(&Value);

    #[test]
    fn schema_examples_parse() {
        let id = "0b5f1f6e-6a3c-5d1e-9f1a-2b3c4d5e6f70";
        let examples: Vec<(&str, Value, Check)> = vec![
            ("library_info", json!({}), |v| {
                assert!(v.as_object().unwrap().is_empty())
            }),
            (
                "folder_list",
                json!({"path": "2025", "cursor": "c", "limit": 50}),
                parses::<FolderListParams>,
            ),
            ("media_get", json!({"id": id}), parses::<MediaGetParams>),
            (
                "media_update",
                json!({"id": id, "rating": 4, "subjects": ["cat"],
                       "hierarchical_subjects": ["animal|cat"],
                       "description": "A cat.", "label": null}),
                parses::<MediaUpdateParams>,
            ),
            (
                "organize_apply",
                json!({"ops": [{"item_id": id, "kind": "move", "to": "2025/a.jpg",
                                "file_hash": "blake3:00"},
                               {"item_id": id, "kind": "trash"}],
                       "label": "by year", "dry_run": true}),
                parses::<OrganizeApplyParams>,
            ),
            (
                "organize_status",
                json!({"batch_id": "b"}),
                parses::<OrganizeStatusParams>,
            ),
            (
                "organize_undo",
                json!({"batch_id": "b", "dry_run": true}),
                parses::<OrganizeUndoParams>,
            ),
            (
                "organize_history",
                json!({"limit": 10, "cursor": "c"}),
                parses::<OrganizeHistoryParams>,
            ),
            ("media_preview", json!({"ids": [id]}), |v| {
                assert_eq!(v["ids"].as_array().unwrap().len(), 1)
            }),
        ];
        let tools = framework_tools();
        assert_eq!(examples.len(), tools.len());
        for (name, example, check) in examples {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            check(&example);
            let props = tool.input_schema["properties"].as_object().unwrap();
            for key in example.as_object().unwrap().keys() {
                assert!(
                    props.contains_key(key),
                    "{name}: {key} is not in the schema"
                );
            }
            for key in tool.input_schema["required"].as_array().unwrap() {
                assert!(props.contains_key(key.as_str().unwrap()), "{name}: {key}");
            }
            assert_eq!(tool.input_schema["additionalProperties"], false, "{name}");
        }
    }

    struct Shadowing;
    impl AppExtension for Shadowing {
        fn accepts(&self, _kind: MediaKind) -> bool {
            true
        }
        fn mcp_tools(&self) -> Vec<McpTool> {
            let tool = |name: &str, method: &str| McpTool {
                name: name.into(),
                description: "app".into(),
                input_schema: json!({"type": "object"}).as_object().unwrap().clone(),
                method: method.into(),
            };
            vec![
                tool("media_get", "album.get"),
                tool("album_list", "album.list"),
                tool("sneaky", "upload.commit"),
                tool("rescan", "library.rescan"),
            ]
        }
    }

    #[test]
    fn app_tools_cannot_shadow_framework_tools() {
        let tools = tool_table(&Shadowing);
        let gets: Vec<&McpTool> = tools.iter().filter(|t| t.name == "media_get").collect();
        assert_eq!(gets.len(), 1);
        assert_eq!(gets[0].method, "media.get");
        let album = tools.iter().find(|t| t.name == "album_list").unwrap();
        assert_eq!(album.method, "album.list");
        assert!(
            !tools
                .iter()
                .any(|t| t.name == "sneaky" || t.name == "rescan")
        );
        assert_eq!(tools.len(), 10);
    }
}
