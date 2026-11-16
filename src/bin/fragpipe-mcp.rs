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
    let mut prefix = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--fragpipe-argv" {
            prefix.push(iter.next().ok_or_else(|| anyhow::anyhow!("--fragpipe-argv requires a value"))?.clone());
        } else if let Some(value) = arg.strip_prefix("--fragpipe-argv=") {
            prefix.push(value.to_owned());
        } else {
            anyhow::bail!("unknown argument: {arg}");
        }
    }
    if prefix.is_empty() {
        if let Ok(value) = std::env::var("FRAGPIPE_ARGV") { prefix.extend(value.split_whitespace().map(str::to_owned)); }
        if prefix.is_empty() {
            let sibling = exe.with_file_name("fragpipe");
            prefix.push(if sibling.is_file() { sibling.to_string_lossy().into_owned() } else { "fragpipe".into() });
        }
    }
    anyhow::ensure!(!prefix[0].is_empty(), "fragpipe executable must not be empty");
    let fragpipe = FragpipeCtx {
        project_root: project_root.clone(),
        fragpipe_argv: prefix,
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
