use fragpipe::mcp::{FragpipeCtx, cluster, layer, layered};
use layer::{job_tools::JobTools, jobs::Jobs};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    if layer::jobs::run_if_internal()? {
        return Ok(());
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] || args == ["-h"] {
        println!(
            "fragpipe-mcp\n\nMCP stdio server for fragpipe.\n\nRun without arguments from an MCP client."
        );
        return Ok(());
    }
    let project_root = std::env::current_dir()?;
    let exe = std::env::current_exe()?;
    let jobs = Jobs::new(cluster::jobs_dir(&project_root), &exe);
    if let [job, watch, id] = args.as_slice()
        && job == "job"
        && watch == "watch"
    {
        std::process::exit(layer::jobs::watch_to_exit(&jobs, id)?);
    }
    anyhow::ensure!(args.is_empty(), "usage: fragpipe-mcp [job watch <id>]");
    let fragpipe = FragpipeCtx {
        project_root: project_root.clone(),
        fragpipe_bin: "fragpipe".into(),
    };
    let steampipe = cluster::SteampipeCtx {
        project_root,
        cluster_ctl: std::env::var_os("CLUSTER_CTL_BIN")
            .map_or_else(|| "cluster-ctl".into(), Into::into),
    };
    let jobs = JobTools {
        jobs,
        registry: Default::default(),
        profile_paths: vec![],
    };
    layered(fragpipe, steampipe, jobs).serve_stdio().await
}
