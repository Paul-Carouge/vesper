//! One Vesper per session: with the window closed to the tray, launching it
//! again must not start a second, hidden replay buffer. The first instance
//! owns the D-Bus name `APP_ID`; a later launch asks it to show its window,
//! then exits.

use ashpd::zbus::fdo::{RequestNameFlags, RequestNameReply};
use ashpd::zbus::{self, Connection, interface};
use futures_lite::future::block_on;

use crate::shortcuts::APP_ID;

const PATH: &str = "/io/github/PaulCarouge/Vesper";

struct Instance {
    on_show: Box<dyn Fn() + Send + Sync>,
}

// `crate`: zbus comes through ashpd, not as a direct dependency.
#[interface(name = "io.github.PaulCarouge.Vesper", crate = "ashpd::zbus")]
impl Instance {
    fn show(&self) {
        (self.on_show)();
    }
}

pub enum Claim {
    /// This is the only instance; keep the connection for its lifetime.
    First(Option<Connection>),
    /// Another instance runs and was asked to show its window.
    Running,
}

/// Claims the name, serving `on_show` (called on a D-Bus thread). Without a
/// session bus, runs anyway, unguarded.
pub fn claim(on_show: impl Fn() + Send + Sync + 'static) -> Claim {
    match block_on(try_claim(Box::new(on_show))) {
        Ok(claim) => claim,
        Err(e) => {
            log!("single instance check failed ({e}); running anyway");
            Claim::First(None)
        }
    }
}

async fn try_claim(on_show: Box<dyn Fn() + Send + Sync>) -> ashpd::zbus::Result<Claim> {
    let conn = Connection::session().await?;
    // Served before the name is taken, so a Show never finds it missing.
    conn.object_server().at(PATH, Instance { on_show }).await?;
    let reply = conn
        .request_name_with_flags(APP_ID, RequestNameFlags::DoNotQueue.into())
        .await;
    match reply {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Claim::First(Some(conn)))
        }
        Ok(_) | Err(ashpd::zbus::Error::NameTaken) => {
            if let Err(e) = conn
                .call_method(Some(APP_ID), PATH, Some(APP_ID), "Show", &())
                .await
            {
                log!("could not show the running instance: {e}");
            }
            Ok(Claim::Running)
        }
        Err(e) => Err(e),
    }
}
