//! All modules are `pub(crate)` internals of the tools subsystem; the
//! crate facade (crate root) re-exports only the public tool surface.

pub(crate) mod antigravity_image_tool;
pub(crate) mod antigravity_search_tool;
pub(crate) mod bash;
pub(crate) mod bash_guard;
pub(crate) mod bash_local;
// Rendering preview helpers: unused by the engine's headless core
// (pa-tui owns terminal rendering); lives here for parity tests.
#[allow(dead_code)]
pub(crate) mod code_preview;
pub(crate) mod code_preview_python;
pub(crate) mod edit;
pub(crate) mod edit_diff;
pub(crate) mod file_mutation_queue;
pub(crate) mod golden_replay;
pub(crate) mod ipython;
pub(crate) mod ipython_cell_code;
pub(crate) mod jsdiff;
pub(crate) mod output_accumulator;
#[allow(dead_code)] // util module; every fn exercised by unit + golden tests
pub(crate) mod path_utils;
#[allow(dead_code)] // render helpers; consumed by the TUI-side renderer (parity-ported here)
pub(crate) mod render_utils;
pub(crate) mod rlm_bootstrap;
#[allow(dead_code)] // util module; fully exercised by unit + golden tests
pub(crate) mod shell_utils;
pub(crate) mod tool_definition;
#[allow(dead_code)] // util module; fully exercised by unit + golden tests
pub(crate) mod truncate;


/// The optional Antigravity model tools (`google_search`, `generate_image`), bridged for the
/// agent runtime and filtered by the extension's extra-tool toggles
/// (`ANTIGRAVITY_NO_EXTRA_TOOLS` / `ANTIGRAVITY_NO_<TOOL>_TOOL`). Hosts (the daemon worker and
/// the CLI print runtime) register these in the session tool set with the session's working
/// directory and agent dir; the engine still adds the kernel-backed `python_repl` tool itself.
#[must_use]
pub fn antigravity_agent_tools(
    cwd: Option<&std::path::Path>,
    agent_dir: Option<&std::path::Path>,
) -> Vec<std::sync::Arc<dyn pa_agent::types::AgentTool>> {
    use crate::session_engine::tool_bridge::bridge_tool;
    let mut tools: Vec<std::sync::Arc<dyn pa_agent::types::AgentTool>> = Vec::new();
    if antigravity_search_tool::is_search_tool_enabled() {
        let context = antigravity_search_tool::AntigravitySearchToolContext {
            agent_dir: agent_dir.map(std::path::Path::to_path_buf),
            ..antigravity_search_tool::AntigravitySearchToolContext::default()
        };
        tools.push(bridge_tool(
            antigravity_search_tool::antigravity_search_tool_with_context(context),
        ));
    }
    if antigravity_image_tool::is_image_tool_enabled() {
        let context = antigravity_image_tool::AntigravityImageToolContext {
            cwd: cwd.map_or_else(
                || ".".to_string(),
                |dir| dir.to_string_lossy().into_owned(),
            ),
            agent_dir: agent_dir.map(std::path::Path::to_path_buf),
            ..antigravity_image_tool::AntigravityImageToolContext::default()
        };
        tools.push(bridge_tool(
            antigravity_image_tool::antigravity_image_tool_with_context(context),
        ));
    }
    tools
}
