use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::stdio,
};
use serde_json::Value;

use crate::tools;

#[derive(Clone)]
struct NonautTools {
    workspace: PathBuf,
    catalog: Vec<Tool>,
}

impl NonautTools {
    fn new(workspace: PathBuf) -> Result<Self> {
        let catalog = tools::specs()
            .as_array()
            .context("tool catalog is not an array")?
            .iter()
            .map(|spec| {
                let name = spec["name"]
                    .as_str()
                    .context("tool name is missing")?
                    .to_owned();
                let description = spec["description"]
                    .as_str()
                    .context("tool description is missing")?
                    .to_owned();
                let schema = spec["inputSchema"]
                    .as_object()
                    .context("tool input schema is not an object")?
                    .clone();
                Ok(Tool::new(name, description, Arc::new(schema)))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { workspace, catalog })
    }
}

impl ServerHandler for NonautTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Nonaut tools. Every call is enforced by an immutable policy-pinned nono sandbox.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.catalog.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = match tools::execute(&self.workspace, &request.name, &arguments).await {
            Ok(output) => CallToolResult::success(vec![ContentBlock::text(output)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(format!("{error:#}"))]),
        };
        Ok(result.into())
    }
}

pub(crate) async fn run(workspace: PathBuf) -> Result<()> {
    let server = NonautTools::new(workspace)?
        .serve(stdio())
        .await
        .context("could not start the Nonaut MCP server")?;
    server
        .waiting()
        .await
        .context("Nonaut MCP server stopped unexpectedly")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_shared_tool_catalog() {
        let server = NonautTools::new(PathBuf::from("/workspace")).unwrap();
        let expected = tools::specs().as_array().unwrap().len();
        assert_eq!(server.catalog.len(), expected);
        assert!(server.catalog.iter().any(|tool| tool.name == "run_command"));
    }
}
