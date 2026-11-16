use std::path::PathBuf;
use std::process::Command;

pub use mcp_layer as layer;
use mcp_layer::{Layered, Routed, ToolSet, job_tools::JobTools};
use rmcp::{handler::server::wrapper::Parameters, model::*, tool, tool_router};
pub use steampipe::mcp as cluster;

pub mod runners;

#[derive(Clone, Debug)]
pub struct FragpipeCtx {
    pub project_root: PathBuf,
    pub fragpipe_argv: Vec<String>,
}

pub struct FragpipeTools { ctx: FragpipeCtx }

pub fn tool_set(ctx: FragpipeCtx) -> impl ToolSet {
    Routed { state: FragpipeTools { ctx }, router: FragpipeTools::tool_router() }
}

pub fn layered(fragpipe: FragpipeCtx, steampipe: cluster::SteampipeCtx, mut jobs: JobTools) -> Layered {
    jobs.profile_paths.insert(0, fragpipe.project_root.join("fragpipe.toml"));
    runners::register(std::sync::Arc::make_mut(&mut jobs.registry), &fragpipe)
        .expect("duplicate fragpipe runner");
    cluster::layered(steampipe, jobs).with(tool_set(fragpipe))
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AndroidDoctorInput {
    config: Option<String>, device: Option<bool>, adb_serial: Option<String>,
    slot: Option<String>, workers_config: Option<String>,
    /// Seconds before the doctor is killed (default 120, at most 600).
    timeout: Option<u64>,
}

#[tool_router]
impl FragpipeTools {
    #[tool(description = "Run fragpipe android-doctor against an Android config.")]
    fn android_doctor(&self, Parameters(input): Parameters<AndroidDoctorInput>) -> Result<CallToolResult, ErrorData> {
        let (program, prefix) = self.ctx.fragpipe_argv.split_first().ok_or_else(|| ErrorData::internal_error("empty fragpipe argv", None))?;
        let mut cmd = Command::new(program);
        cmd.args(prefix);
        cmd.current_dir(&self.ctx.project_root).arg("android-doctor");
        for (name, value) in [("config", input.config), ("adb-serial", input.adb_serial),
            ("slot", input.slot), ("workers-config", input.workers_config)] {
            if let Some(value) = value { cmd.arg(format!("--{name}={value}")); }
        }
        if input.device == Some(true) { cmd.arg("--device"); }
        let internal = |e: std::io::Error| ErrorData::internal_error(e.to_string(), None);
        let limit = std::time::Duration::from_secs(input.timeout.unwrap_or(120).min(600));
        let mut child = {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(internal)?
        };
        // Drain both pipes on threads so a chatty doctor cannot block on a full pipe.
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let read = |pipe: &mut dyn std::io::Read| {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        };
        let (mut text, status) = std::thread::scope(|scope| {
            let out = scope.spawn(|| read(&mut stdout));
            let err = scope.spawn(|| read(&mut stderr));
            let deadline = std::time::Instant::now() + limit;
            let status = loop {
                if let Some(status) = child.try_wait().map_err(internal)? {
                    break Some(status);
                }
                if std::time::Instant::now() >= deadline {
                    // Kill the doctor's whole group so an adb child cannot outlive it.
                    if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
                        let _ = rustix::process::kill_process_group(
                            group,
                            rustix::process::Signal::KILL,
                        );
                    }
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            };
            Ok::<_, ErrorData>((
                out.join().unwrap_or_default() + &err.join().unwrap_or_default(),
                status,
            ))
        })?;
        text.push_str(match status {
            Some(status) if status.success() => "\n--- PASS ---\n",
            Some(_) => "\n--- FAIL ---\n",
            None => "\n--- TIMEOUT ---\n",
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}
