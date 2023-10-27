use futures::{AsyncRead, AsyncWrite};
use tokio::{process::Child, sync::oneshot, task::JoinHandle};
use tokio_util::codec::Framed;

/// A handle to a session which can be used to attach new clients to the session.
pub struct SessionHandsalxe {}

pub struct SessionPort {}

impl SessionHandle {
    // pub fn attach(&self, )
}

fn spawn_server<T: AsyncRead + AsyncWrite>(io: T) {}
