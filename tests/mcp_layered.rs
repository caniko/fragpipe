#![cfg(feature = "mcp")]

use fragpipe::mcp::{FragpipeCtx, layered};
use mcp_layer::{job_tools::JobTools, jobs::Jobs};
use rmcp::{ServiceExt, model::CallToolRequestParams};

#[tokio::test]
async fn inventory_and_history_use_project_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("steampipe.toml"),
        format!(
            "binary_name='fixture'\ncargo_package='fixture'\nvm_user='fixture'\n\
             remote_dir='/home/fixture/work'\nlock_dir='{}'\n[cluster]\nstate_dir='{}'\n",
            dir.path().join("locks").display(),
            dir.path().join("state").display(),
        ),
    )
    .unwrap();
    let jobs = JobTools {
        jobs: Jobs::new(dir.path().join("jobs"), "unused"),
        registry: Default::default(),
        profile_paths: vec![],
    };
    let fragpipe = FragpipeCtx {
        project_root: dir.path().to_owned(),
        fragpipe_argv: vec!["fragpipe".into()],
    };
    let steampipe = steampipe::mcp::SteampipeCtx {
        project_root: dir.path().to_owned(),
        cluster_ctl: "cluster-ctl".into(),
    };
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = tokio::spawn(async move {
        layered(fragpipe, steampipe, jobs)
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(client_io).await.unwrap();
    let names: Vec<_> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert_eq!(
        names,
        [
            "android_doctor",
            "cluster_history",
            "cluster_logs",
            "cluster_status",
            "cluster_stop",
            "job_cancel",
            "job_list",
            "job_logs",
            "job_profiles",
            "job_start",
            "job_watch",
        ]
    );
    let response = client
        .call_tool(CallToolRequestParams::new("cluster_history").with_arguments(Default::default()))
        .await
        .unwrap();
    assert_eq!(
        response.content[0].as_text().unwrap().text,
        "No test history found."
    );
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn tournament_jobs_and_profiles() {
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;

    if std::env::var_os("FRAGPIPE_MCP_TEST_CHILD").is_none() {
        for display in [true, false] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "tournament_jobs_and_profiles", "--nocapture"])
                .env(
                    "FRAGPIPE_MCP_TEST_CHILD",
                    if display { "display" } else { "headless" },
                )
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DISPLAY");
            if display {
                child.env("DISPLAY", ":99");
            }
            assert!(child.status().unwrap().success());
        }
        return;
    }
    let display = std::env::var("FRAGPIPE_MCP_TEST_CHILD").unwrap() == "display";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fragpipe.toml"), "[tournament]\nport=27100\n[[remote]]\nname='nomad'\nsession_user='tester'\n[job_profiles.lan-test]\nrunner='direct-tournament'\n[job_profiles.lan-test.params]\nremote='nomad'\n").unwrap();
    let script = dir.path().join("fake-cli");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\n' \"$@\"\ncase \"$*\" in *sleepy*) sleep 3;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let jobs = JobTools {
        jobs: Jobs::new(dir.path().join("jobs"), env!("CARGO_BIN_EXE_fragpipe-mcp")),
        registry: Default::default(),
        profile_paths: vec![],
    };
    let f = FragpipeCtx {
        project_root: dir.path().into(),
        fragpipe_argv: vec![script.to_string_lossy().into()],
    };
    let s = steampipe::mcp::SteampipeCtx {
        project_root: dir.path().into(),
        cluster_ctl: "cluster-ctl".into(),
    };
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = tokio::spawn(async move {
        layered(f, s, jobs)
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_io).await.unwrap();
    let call = |name: &'static str, args: Value| {
        CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone())
    };
    let profiles = client
        .call_tool(call("job_profiles", json!({})))
        .await
        .unwrap();
    assert!(
        profiles.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("lan-test")
    );
    let start = |extra: Value| {
        call(
            "job_start",
            json!({"spec":{"runner":"direct-tournament", "params":extra}}),
        )
    };
    let params = json!({"config":"fragpipe.toml", "remote":"nomad", "windowed":true, "host_extra_args":["--lobby-start"]});
    let response = client.call_tool(start(params)).await;
    if display {
        let response = response.unwrap();
        let id: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
        let watched = client
            .call_tool(call("job_watch", json!({"id":id["id"],"wait_secs":10})))
            .await
            .unwrap();
        let watched: Value =
            serde_json::from_str(&watched.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(watched["verdict"]["result"], "pass");
        let output = watched["new_lines"].to_string();
        for arg in [
            "direct-tournament",
            "--drop-args=--headless",
            "--host-extra-args=--lobby-start",
        ] {
            assert!(output.contains(arg), "{output}");
        }
        let first = client
            .call_tool(start(json!({"remote":"sleepy"})))
            .await
            .unwrap();
        let id: Value = serde_json::from_str(&first.content[0].as_text().unwrap().text).unwrap();
        assert!(
            client
                .call_tool(start(json!({"remote":"nomad"})))
                .await
                .is_err()
        );
        let watched = client
            .call_tool(call("job_watch", json!({"id":id["id"],"wait_secs":10})))
            .await
            .unwrap();
        let watched: Value =
            serde_json::from_str(&watched.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(watched["verdict"]["result"], "pass");
    } else {
        assert!(response.is_err());
    }
    assert!(
        client
            .call_tool(start(
                json!({"remote":"nomad", "host_extra_args":["bad arg"]})
            ))
            .await
            .is_err()
    );
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn android_doctor_is_killed_at_its_timeout() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-cli");
    std::fs::write(&script, "#!/bin/sh\necho doctor-started\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let jobs = JobTools {
        jobs: Jobs::new(dir.path().join("jobs"), "unused"),
        registry: Default::default(),
        profile_paths: vec![],
    };
    let fragpipe = FragpipeCtx {
        project_root: dir.path().to_owned(),
        fragpipe_argv: vec![script.to_string_lossy().into()],
    };
    let steampipe = steampipe::mcp::SteampipeCtx {
        project_root: dir.path().to_owned(),
        cluster_ctl: "cluster-ctl".into(),
    };
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = tokio::spawn(async move {
        layered(fragpipe, steampipe, jobs)
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_io).await.unwrap();
    let started = std::time::Instant::now();
    let result = client
        .call_tool(
            CallToolRequestParams::new("android_doctor").with_arguments(
                serde_json::json!({"timeout": 1})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(
        text.contains("doctor-started") && text.contains("--- TIMEOUT ---"),
        "{text}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    client.cancel().await.unwrap();
    server.await.unwrap();
}
