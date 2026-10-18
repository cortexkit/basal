use std::time::Duration;

use subc_protocol::session::{HealthStatus, ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{read_frame, write_frame};
use tokio::net::TcpStream;

/// Observe a greeted worker or the first failed spawn through the module's
/// health replies. A missing worker must not leave a process-image wait pending
/// until the outer test deadline, because the module stays alive in backoff.
pub async fn wait_for_worker_startup(
    stream: &mut TcpStream,
    timeout: Duration,
) -> Result<(), String> {
    tokio::time::timeout(timeout, async {
        loop {
            let request = Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                42,
                serde_json::to_vec(&ModuleControlRequest::HealthCheck {})
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            write_frame(stream, &request)
                .await
                .map_err(|e| e.to_string())?;
            let reply = read_frame(stream)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("module exited before a worker greeted")?;
            if reply.header.ty != FrameType::Response || reply.header.corr != 42 {
                return Err(format!(
                    "unexpected worker startup frame: {:?}",
                    reply.header
                ));
            }
            let report: ModuleControlResponse =
                serde_json::from_slice(&reply.body).map_err(|e| e.to_string())?;
            let ModuleControlResponse::HealthCheck {
                status,
                detail,
                metrics,
            } = report
            else {
                return Err(format!("unexpected worker startup response: {report:?}"));
            };
            if status == HealthStatus::Failing {
                return Err(format!("module startup failed: {detail:?}"));
            }
            let Some(metrics) = metrics else { continue };
            let workers = &metrics["workers"];
            let failures = workers["spawn_failures"]
                .as_u64()
                .ok_or("health omitted worker spawn failures")?;
            if failures != 0 {
                return Err(format!("worker startup failed: {detail:?}; {metrics}"));
            }
            if workers["spawned"]
                .as_u64()
                .ok_or("health omitted greeted workers")?
                != 0
            {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| "worker startup health deadline elapsed".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpListener;

    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    async fn answer(stream: &mut TcpStream, metrics: Option<serde_json::Value>) {
        let request = read_frame(stream).await.unwrap().unwrap();
        assert_eq!(request.header.ty, FrameType::Request);
        let body: ModuleControlRequest = serde_json::from_slice(&request.body).unwrap();
        assert!(matches!(body, ModuleControlRequest::HealthCheck {}));
        let reply = Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&ModuleControlResponse::HealthCheck {
                status: HealthStatus::Degraded,
                detail: Some("worker fixture".into()),
                metrics,
            })
            .unwrap(),
        )
        .unwrap();
        write_frame(stream, &reply).await.unwrap();
    }

    #[tokio::test]
    async fn waits_for_a_greeted_worker_through_startup_health() {
        let (mut client, mut server) = pair().await;
        let daemon = tokio::spawn(async move {
            answer(&mut server, None).await;
            answer(
                &mut server,
                Some(json!({"workers": {"spawned": 0, "spawn_failures": 0}})),
            )
            .await;
            answer(
                &mut server,
                Some(json!({"workers": {"spawned": 1, "spawn_failures": 0}})),
            )
            .await;
        });
        wait_for_worker_startup(&mut client, Duration::from_secs(10))
            .await
            .unwrap();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_the_first_spawn_failure_even_with_a_greeted_worker() {
        let (mut client, mut server) = pair().await;
        let daemon = tokio::spawn(async move {
            answer(&mut server, None).await;
            answer(
                &mut server,
                Some(json!({"workers": {"spawned": 1, "spawn_failures": 1}})),
            )
            .await;
        });
        let error = wait_for_worker_startup(&mut client, Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(error.contains("worker startup failed"), "{error}");
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn bounds_an_unanswered_health_check() {
        let (mut client, mut server) = pair().await;
        let (done, held) = tokio::sync::oneshot::channel::<()>();
        let daemon = tokio::spawn(async move {
            read_frame(&mut server).await.unwrap().unwrap();
            held.await.unwrap();
        });
        let error = wait_for_worker_startup(&mut client, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error, "worker startup health deadline elapsed");
        done.send(()).unwrap();
        daemon.await.unwrap();
    }
}
