use anyhow::{bail, Context};
use lsp_server::{Message, RequestId};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Given any input that can be treated as a `PathBuf` walk the directories up to the root and
/// returns the path directory that can be treated as a session root.
pub fn find_project_root(path: impl Into<PathBuf>) -> anyhow::Result<PathBuf> {
    let mut result = None;
    let mut current: PathBuf = path.into();
    let param = current.clone(); // To report error.

    assert!(
        current.is_absolute(),
        "Expected project path to be an absolute path."
    );

    loop {
        current.push("Cargo.lock");
        let exists = current.is_file();
        current.pop();

        if exists {
            // Cargo.lock is a hard stop for us. It means we're at a crate.
            return Ok(current);
        }

        // With cargo.toml we look for the deepest one since the project can be a work space.
        // This will also lead us to find the `Cargo.lock` of the entire workspace.
        current.push("Cargo.toml");
        let exists = current.is_file();
        current.pop();
        if exists {
            result = Some(current.clone());
        }

        // The `.rad_root` file can be used as a hack to pin where the root of the file
        // system is.
        current.push(".rad_root");
        let exists = current.is_file();
        current.pop();
        if exists {
            break;
        }

        // we have reached the root directory and there is no more parent for us to visit.
        if !current.pop() {
            break;
        }
    }

    if result.is_none() {
        bail!(
            "Could not find the project root for {:?}.",
            param.to_string_lossy()
        );
    }

    Ok(result.unwrap())
}

/// Read an LSP message from a [`BufReader`].
pub async fn read_lsp_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> anyhow::Result<Option<lsp_server::Message>> {
    let mut read_buf = String::new();
    // read headers.
    let mut size = None;
    loop {
        read_buf.clear();
        if reader.read_line(&mut read_buf).await? == 0 {
            return Ok(None);
        }
        if !read_buf.ends_with("\r\n") {
            bail!("malformed header: {read_buf:?}");
        }
        let buf = &read_buf[..read_buf.len() - 2];
        if buf.is_empty() {
            break;
        }
        let mut parts = buf.splitn(2, ": ");
        let header_name = parts.next().unwrap();
        let header_value = parts
            .next()
            .with_context(|| format!("malformed header: {read_buf:?}"))?;
        if header_name.eq_ignore_ascii_case("Content-Length") {
            size = Some(
                header_value
                    .parse::<usize>()
                    .context("invalid Content-Length")?,
            );
        }
    }

    let size = size.ok_or_else(|| anyhow::anyhow!("no Content-Length"))?;

    let mut buf = vec![0; size];
    reader.read_exact(&mut buf).await?;
    let buf = String::from_utf8(buf).context("Invalid UTF-8")?;
    let message: lsp_server::Message = serde_json::from_str(&buf).context("invalid message")?;
    Ok(Some(message))
}

/// Encode a LSP message for JSON RPC. (with the `Content-Length` header and stuff)
pub fn encode_lsp_message(message: lsp_server::Message) -> Vec<u8> {
    let mut result = Vec::new();
    message.write(&mut result).unwrap();
    result
}

pub fn bind_transport<T: AsyncRead + AsyncWrite>(io: T) -> Framed<T, LengthDelimitedCodec> {
    Framed::new(io, LengthDelimitedCodec::new())
}

pub fn get_id(message: &Message) -> Option<&RequestId> {
    match message {
        Message::Request(req) => Some(&req.id),
        Message::Response(res) => Some(&res.id),
        Message::Notification(_) => None,
    }
}
