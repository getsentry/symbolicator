use std::fs::File;
use std::pin::pin;

use anyhow::Context;
use serde::Deserialize;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::io::StreamReader;

#[derive(Deserialize)]
pub struct StoredAttachment {
    organization_id: u64,
    project_id: u64,
    stored_id: String,
}

pub async fn fetch_attachment_into_file(
    attachment_store: &objectstore_client::ClientBuilder,
    attachment: StoredAttachment,
    file: File,
) -> anyhow::Result<File> {
    let attachments_store =
        attachment_store.for_project(attachment.organization_id, attachment.project_id);
    let attachment = attachments_store
        .get(&attachment.stored_id)
        .send()
        .await?
        .context("expected attachment")?;

    let mut reader = pin!(StreamReader::new(attachment.stream));
    let mut writer = BufWriter::new(tokio::fs::File::from_std(file));
    tokio::io::copy(&mut reader, &mut writer).await?;
    writer.flush().await?;
    let file = writer.into_inner();
    file.sync_data().await?;

    Ok(file.into_std().await)
}
