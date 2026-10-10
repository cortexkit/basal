//! The durable core-scope handshake is separate from worker dispatch. Recovery
//! may repeat an idempotent open to learn a lost epoch, but never a tool call.

use basal_host::run_scope::{self, Close, Open, Opened};
use basal_host::transport::{Transport, WireError};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::{CoreError, Store};

pub fn close(store: &Store, transport: &dyn Transport, id: &str) -> crate::error::Result<()> {
    let saved = store.read(|conn| {
        Ok(conn
            .query_row(
                "SELECT request,opened,refused,closed FROM codemode_scopes WHERE run_id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )
            .optional()?)
    })?;
    let Some((request, opened, refused, closed)) = saved else {
        return Ok(());
    };
    if closed || refused {
        return Ok(());
    }
    let opened: Opened = match opened {
        Some(opened) => {
            serde_json::from_str(&opened).map_err(|e| CoreError::Corrupt(e.to_string()))?
        }
        None => {
            let request: Open =
                serde_json::from_str(&request).map_err(|e| CoreError::Corrupt(e.to_string()))?;
            let opened = run_scope::open(transport, &request)
                .map_err(|e| CoreError::Invalid(format!("recovering run scope: {e:?}")))?;
            store.write(|tx| {
                tx.execute(
                    "UPDATE codemode_scopes SET opened=?2 WHERE run_id=?1",
                    rusqlite::params![id, serde_json::to_string(&opened).unwrap()],
                )?;
                Ok(())
            })?;
            opened
        }
    };
    let closed = run_scope::close(
        transport,
        &Close {
            run_id: id.into(),
            epoch: opened.scope.epoch,
        },
    )
    .map_err(|e| CoreError::Invalid(format!("closing run scope: {e:?}")))?;
    if !closed.closed && !closed.already_closed {
        return Err(CoreError::Invalid(
            "core did not confirm that the run scope closed".into(),
        ));
    }
    store.write(|tx| {
        tx.execute("UPDATE codemode_scopes SET closed=1 WHERE run_id=?1", [id])?;
        Ok(())
    })
}

/// Unknown core codes and details are data, not a second refusal vocabulary.
pub fn error_value(error: WireError) -> Value {
    match error {
        WireError::Refused { code, message } => json!({"code":code,"message":message}),
        WireError::RefusedDetails {
            code,
            message,
            detail,
        } => json!({"code":code,"message":message,"detail":detail}),
        other => json!({"code":"codemode_unavailable","message":format!("{other:?}")}),
    }
}
