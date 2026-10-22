//! Acceptance tests of `sluice mcp`: an rmcp client spawns the built binary
//! over stdio with the sandbox environment (its own HOME, XDG dirs and
//! daemon socket) and calls the tools.

mod common;

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::Sandbox;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, NumberOrString, ProgressNotificationParam,
    ProgressToken, RequestMetaObject,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};

/// Client handler that counts progress notifications.
#[derive(Debug, Clone, Default)]
struct Counter(Arc<AtomicUsize>);

impl ClientHandler for Counter {
    async fn on_progress(
        &self,
        _params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

type Client = RunningService<RoleClient, Counter>;

async fn connect(sb: &Sandbox, counter: Counter) -> Client {
    connect_with(sb, counter, &[]).await
}

async fn connect_with(sb: &Sandbox, counter: Counter, extra_env: &[(&str, &str)]) -> Client {
    let mut cmd = tokio::process::Command::new(Sandbox::binary());
    cmd.arg("mcp")
        .current_dir(&sb.home)
        .env_clear()
        .envs(sb.env())
        .envs(extra_env.iter().copied());
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sluice mcp");
    counter.serve(transport).await.expect("initialize")
}

fn params(name: &'static str, args: &Value) -> CallToolRequestParams {
    let Value::Object(map) = args.clone() else {
        panic!("arguments must be an object");
    };
    CallToolRequestParams::new(name).with_arguments(map)
}

async fn call(client: &Client, name: &'static str, args: &Value) -> CallToolResult {
    client
        .call_tool(params(name, args))
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e}"))
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn structured(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structured_content")
}

fn repo_arg(sb: &Sandbox) -> String {
    sb.repo.display().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_list_verify_status_and_logs() {
    let sb = Sandbox::standard();
    let client = connect(&sb, Counter::default()).await;
    let dir = repo_arg(&sb);

    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    for t in ["verify", "status", "gate_list", "logs"] {
        assert!(tools.contains(&t.to_owned()), "{tools:?}");
    }
    let instructions = client
        .peer_info()
        .and_then(|i| i.instructions.clone())
        .unwrap_or_default();
    assert!(instructions.contains("verify"), "{instructions}");

    // gate_list
    let r = call(&client, "gate_list", &json!({"working_directory": dir})).await;
    let names: Vec<&str> = structured(&r)["gates"]
        .as_array()
        .expect("gates")
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fail", "lint", "slow", "test"]);
    assert!(text(&r).contains("echo run"), "{}", text(&r));

    // verify: miss, then hit
    let args = json!({"working_directory": dir, "gates": ["test"]});
    let r = call(&client, "verify", &args).await;
    assert_eq!(r.is_error, Some(false), "{}", text(&r));
    assert!(text(&r).starts_with("test  PASS "), "{}", text(&r));
    let s = structured(&r);
    assert_eq!(s["all_passed"], true, "{s}");
    assert_eq!(s["results"][0]["status"], "pass");
    assert_eq!(s["results"][0]["cached"], false);
    assert_eq!(sb.runs(), 1);

    let r = call(&client, "verify", &args).await;
    assert!(text(&r).contains("PASS (cached, passed"), "{}", text(&r));
    assert_eq!(structured(&r)["results"][0]["cached"], true);
    assert_eq!(sb.runs(), 1, "a hit runs nothing");

    // verify: failing gate
    let r = call(
        &client,
        "verify",
        &json!({"working_directory": dir, "gates": ["fail"]}),
    )
    .await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    let s = structured(&r);
    assert_eq!(s["all_passed"], false);
    assert_eq!(s["results"][0]["status"], "fail");
    assert_eq!(s["results"][0]["exit_code"], 7);
    let tail = s["results"][0]["tail"].to_string();
    assert!(tail.contains("boom"), "{s}");
    assert!(text(&r).contains("FAIL exit=7"), "{}", text(&r));
    assert!(text(&r).contains("--- last "), "{}", text(&r));
    let fail_job = s["results"][0]["job_id"]
        .as_str()
        .expect("job id")
        .to_owned();

    // status, with the cache state of the tree
    let r = call(&client, "status", &json!({"working_directory": dir})).await;
    let s = structured(&r);
    assert_eq!(s["schema"], 1, "{s}");
    assert!(s["daemon"].is_object(), "{s}");
    let test_gate = s["cache"]["gates"]
        .as_array()
        .expect("cache gates")
        .iter()
        .find(|g| g["gate"] == "test")
        .expect("test gate")
        .clone();
    assert_eq!(test_gate["state"], "hit", "{s}");
    assert!(text(&r).contains("budget:"), "{}", text(&r));

    // logs of the failing job, and of the newest job
    let r = call(&client, "logs", &json!({"job_id": fail_job, "tail": 50})).await;
    let lines = structured(&r)["lines"].to_string();
    assert!(lines.contains("boom"), "{}", text(&r));
    let r = call(&client, "logs", &json!({})).await;
    assert_eq!(r.is_error, Some(false), "{}", text(&r));
    assert_ne!(structured(&r)["lines"], json!([]), "{}", text(&r));

    client.cancel().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests_are_mcp_errors() {
    let sb = Sandbox::standard();
    let client = connect(&sb, Counter::default()).await;
    let relative = client
        .call_tool(params("verify", &json!({"working_directory": "repo"})))
        .await;
    assert!(relative.is_err(), "{relative:?}");
    let no_config = client
        .call_tool(params(
            "verify",
            &json!({"working_directory": sb.home.display().to_string()}),
        ))
        .await;
    assert!(no_config.is_err(), "{no_config:?}");
    let unknown = client
        .call_tool(params(
            "verify",
            &json!({"working_directory": repo_arg(&sb), "gates": ["nope"]}),
        ))
        .await;
    assert!(unknown.is_err(), "{unknown:?}");
    // No daemon was needed for any of these.
    let r = call(&client, "status", &json!({})).await;
    assert_eq!(structured(&r)["daemon"], Value::Null);
    client.cancel().await.expect("shutdown");
}

fn alive(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next().map(str::to_owned))
            .is_some_and(|state| state != "Z")
    })
}

fn wait_for(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

#[tokio::test(flavor = "multi_thread")]
async fn progress_is_sent_and_cancel_kills_the_gate() {
    let sb = Sandbox::new(
        r#"
[gates.nap]
cmd = "sleep 2.5"

[gates.sleeper]
cmd = "echo $$ > \"$SLUICE_TEST_COUNTER.pid\"; exec sleep 30"
"#,
    );
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        "version = 1\n[mcp]\nprogress_interval = \"1s\"\n",
    )
    .expect("user config");
    let counter = Counter::default();
    let client = connect(&sb, counter.clone()).await;
    let dir = repo_arg(&sb);

    // Progress notifications while a gate runs, when a token is supplied.
    let mut p = params(
        "verify",
        &json!({"working_directory": dir, "gates": ["nap"]}),
    );
    let mut meta = RequestMetaObject::default();
    meta.set_progress_token(ProgressToken(NumberOrString::Number(7)));
    p.meta = Some(meta);
    let r = client.call_tool(p).await.expect("verify nap");
    assert_eq!(r.is_error, Some(false), "{}", text(&r));
    assert!(
        counter.0.load(Ordering::SeqCst) >= 1,
        "expected progress notifications"
    );

    // Cancelling the request (dropping the call) kills the gate.
    let peer = client.peer().clone();
    let args = json!({"working_directory": dir, "gates": ["sleeper"]});
    let call = tokio::spawn(async move { peer.call_tool(params("verify", &args)).await });
    let pid_file = format!("{}.pid", sb.counter.display());
    let pid_file = Path::new(&pid_file);
    assert!(
        wait_for(Duration::from_secs(15), || pid_file.exists()
            && fs::read_to_string(pid_file)
                .is_ok_and(|s| !s.trim().is_empty())),
        "the gate never started"
    );
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .expect("pid");
    assert!(alive(pid));
    call.abort();
    assert!(
        wait_for(Duration::from_secs(7), || !alive(pid)),
        "the sleep process survived the cancellation"
    );
    client.cancel().await.expect("shutdown");
}

const EXTRA: &str = r#"
[gates.counted]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\"; true"

[gates.nap]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\"; sleep 1"

[gates.stuck]
cmd = "sleep 5"

[gates.bad]
cmd = "exit 3"

[gates.after]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""

[gates.noisy]
cmd = "seq 1 3000"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn extra_args_are_part_of_the_cache_key() {
    let sb = Sandbox::new(EXTRA);
    let client = connect(&sb, Counter::default()).await;
    let dir = repo_arg(&sb);
    let plain = json!({"working_directory": dir, "gates": ["counted"]});
    let with_x = json!({"working_directory": dir, "gates": ["counted"], "extra_args": ["x"]});

    call(&client, "verify", &plain).await;
    assert_eq!(sb.runs(), 1);
    let r = call(&client, "verify", &with_x).await;
    assert_eq!(r.is_error, Some(false), "{}", text(&r));
    assert_eq!(structured(&r)["results"][0]["cached"], false);
    assert_eq!(sb.runs(), 2, "extra arguments must miss the plain pass");
    let r = call(&client, "verify", &with_x).await;
    assert_eq!(structured(&r)["results"][0]["cached"], true);
    assert_eq!(sb.runs(), 2);
    client.cancel().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_daemon_is_an_error_per_gate_not_an_mcp_error() {
    let sb = Sandbox::new(EXTRA);
    let client = connect_with(
        &sb,
        Counter::default(),
        &[("SLUICE_SOCKET", "/dev/null/impossible")],
    )
    .await;
    let r = client
        .call_tool(params(
            "verify",
            &json!({"working_directory": repo_arg(&sb), "gates": ["counted", "after"]}),
        ))
        .await
        .expect("a Sluice failure is a tool result, not an MCP error");
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    let s = structured(&r);
    assert_eq!(s["all_passed"], false, "{s}");
    let results = s["results"].as_array().expect("results");
    assert_eq!(results.len(), 2, "{s}");
    for g in results {
        assert_eq!(g["status"], "error", "{s}");
        assert!(g["message"].is_string(), "{s}");
    }
    assert_eq!(sb.runs(), 0);
    client.cancel().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_verifies_coalesce_and_a_failure_does_not_stop_the_next_gate() {
    let sb = Sandbox::new(EXTRA);
    let client = connect(&sb, Counter::default()).await;
    let dir = repo_arg(&sb);
    let args = json!({"working_directory": dir, "gates": ["nap"]});
    let (a, b) = tokio::join!(
        call(&client, "verify", &args),
        call(&client, "verify", &args)
    );
    let coalesced = [&a, &b]
        .iter()
        .filter(|r| structured(r)["results"][0]["coalesced"] == true)
        .count();
    assert_eq!(coalesced, 1, "{} / {}", text(&a), text(&b));
    assert_eq!(sb.runs(), 1, "one execution for both calls");

    let before = sb.runs();
    let r = call(
        &client,
        "verify",
        &json!({"working_directory": dir, "gates": ["bad", "after"]}),
    )
    .await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    let s = structured(&r);
    assert_eq!(s["all_passed"], false, "{s}");
    assert_eq!(s["results"][0]["status"], "fail");
    assert_eq!(s["results"][1]["status"], "pass");
    assert_eq!(sb.runs(), before + 1, "the gate after the failure ran");
    client.cancel().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_seconds_and_logs_limits() {
    let sb = Sandbox::new(EXTRA);
    let client = connect(&sb, Counter::default()).await;
    let dir = repo_arg(&sb);
    let r = call(
        &client,
        "verify",
        &json!({"working_directory": dir, "gates": ["stuck"], "timeout_seconds": 1}),
    )
    .await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    assert_eq!(structured(&r)["results"][0]["status"], "timeout");

    let r = call(
        &client,
        "verify",
        &json!({"working_directory": dir, "gates": ["noisy"]}),
    )
    .await;
    let job = structured(&r)["results"][0]["job_id"]
        .as_str()
        .expect("job id")
        .to_owned();
    let r = call(&client, "logs", &json!({"job_id": job, "tail": 5000})).await;
    assert_eq!(r.is_error, Some(false), "{}", text(&r));
    let n = structured(&r)["lines"].as_array().expect("lines").len();
    assert!(n <= 2000 && n > 100, "{n} lines");

    let r = call(&client, "logs", &json!({"job_id": "1-1-deadbeef"})).await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    client.cancel().await.expect("shutdown");
}
