//! jefe as a library: `start_embedded` in this process's runtime, with the
//! protocol as values and control as calls. The only test in its binary,
//! because it points `HOME` and the state directory at a temporary one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::Env;
use jefe::{JobHost, Message};
use jefe_core::{Event, JobSpec, JobStatus};

#[tokio::test(flavor = "multi_thread")]
async fn an_embedded_job_streams_values_and_takes_control() {
    let env = Env::new();
    // SAFETY: this binary runs only this test, and nothing else reads the
    // environment while it is set.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("HOME", env.home());
        std::env::set_var("JEFE_STATE_DIR", env.root.join("state"));
    }
    std::fs::write(env.home().join("hold"), "").unwrap();
    let config = jefe::config::Config::load(&env.root.join("jefe.toml")).unwrap();
    let mut spec = JobSpec::new("m1", "add hello", env.repo().to_str().unwrap());
    spec.workers = config.worker_config();
    spec.attended = true;

    let mut job = jefe::start_embedded(spec, JobHost::local(), None).await.unwrap();
    let mut seen = Vec::new();
    // Ask for a snapshot and nudge once the worker is at work, then release it.
    while let Some(m) = job.messages.recv().await {
        if let Message::Event(s) = &m
            && matches!(s.event, Event::WorkerSpawned { .. })
        {
            job.control.status();
            job.control.nudge("keep it short");
            std::fs::write(env.home().join("go"), "").unwrap();
        }
        let bye = matches!(m, Message::Bye { .. });
        seen.push(m);
        if bye {
            break;
        }
    }
    let report = job.done.await.unwrap().unwrap();
    assert_eq!(report.status, JobStatus::Done);

    assert!(
        matches!(&seen[0], Message::Hello { version: 1, job, .. } if job.as_str() == "m1"),
        "{seen:?}"
    );
    assert_eq!(
        seen.last(),
        Some(&Message::Bye {
            status: JobStatus::Done
        })
    );
    assert!(
        seen.iter()
            .any(|m| matches!(m, Message::Snapshot { job } if job["id"] == "m1"))
    );
    assert!(seen.iter().any(|m| matches!(
        m,
        Message::Event(s) if matches!(&s.event, Event::Notice { message } if message.contains("keep it short"))
    )));
    // Values serialize to the same lines `--json` prints.
    let line = serde_json::to_value(seen.last().unwrap()).unwrap();
    assert_eq!(line, serde_json::json!({"type": "bye", "status": "done"}));
    assert_eq!(
        std::fs::read_to_string(env.repo().join("hello.txt")).unwrap(),
        "hello\n"
    );
}
