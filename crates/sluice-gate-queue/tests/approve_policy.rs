//! Agent approval through the CLI: a `queue-approve` whose client
//! environment is detected as an agent's is refused unless the user config
//! allows agent approval (alpha review finding 2).

mod common;

use common::{Daemon, Fixture, ready, user_config};
use sluice_gate_proto::{
    ApproveVia, ClientMessage, ErrorCode, QueueTrainState as T, ServerMessage,
};

const GATE_TRUE: &str = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n";

async fn approve_as(
    d: &Daemon,
    fx: &Fixture,
    train: &str,
    hash: &str,
    extra: &[(&str, &str)],
) -> ServerMessage {
    let mut env = fx.client_env();
    for (k, v) in extra {
        env.insert((*k).to_owned(), (*v).to_owned());
    }
    d.ask(&ClientMessage::QueueApprove {
        id: 3,
        train_id: train.to_owned(),
        train_hash: hash.to_owned(),
        via: ApproveVia::Cli,
        env,
        pr_landing: true,
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_cannot_approve_through_the_cli_unless_allowed() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("a.txt", "a\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    let hash = t.train_hash.clone().unwrap();
    for agent in [[("CLAUDECODE", "1")], [("SLUICE_AGENT", "1")]] {
        match approve_as(&d, &fx, &t.train_id, &hash, &agent).await {
            ServerMessage::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::NotAllowed);
                assert!(message.contains("allow_agent_approval"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
    // SLUICE_AGENT=0 overrides the detection: a human in an agent terminal.
    match approve_as(
        &d,
        &fx,
        &t.train_id,
        &hash,
        &[("CLAUDECODE", "1"), ("SLUICE_AGENT", "0")],
    )
    .await
    {
        ServerMessage::QueueApproved { train, .. } => assert_eq!(train.state, T::Approved),
        other => panic!("{other:?}"),
    }
    d.wait_train("landed", |x| x.state == T::Landed).await;
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_approves_through_the_cli_when_allowed() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("a.txt", "a\n")]);
    let d = Daemon::start(&fx, user_config(true)).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    let hash = t.train_hash.clone().unwrap();
    let r = approve_as(&d, &fx, &t.train_id, &hash, &[("CLAUDECODE", "1")]).await;
    assert!(matches!(r, ServerMessage::QueueApproved { .. }), "{r:?}");
    d.wait_train("landed", |x| x.state == T::Landed).await;
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), a);
    d.shutdown().await;
}
