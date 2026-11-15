//! Share one provider probe between overlapping slash-command requests for a workspace.

use std::path::{Path, PathBuf};

use crate::HarnessError;
use crate::upstream_proto::SlashCommand;

/// Overlapping callers share one probe, but a later open refreshes, so provider account, plugin
/// and workspace changes are not hidden behind a persistent host cache.
#[derive(Default)]
pub(crate) struct CommandDiscovery {
    latest: tokio::sync::Mutex<Option<(PathBuf, std::time::Instant, Vec<SlashCommand>)>>,
}

impl CommandDiscovery {
    pub(crate) async fn get(
        &self,
        cwd: &Path,
        discover: impl std::future::Future<Output = Result<Vec<SlashCommand>, HarnessError>>,
    ) -> Result<Vec<SlashCommand>, HarnessError> {
        let requested = std::time::Instant::now();
        let mut latest = self.latest.lock().await;
        if let Some((root, completed, commands)) = latest.as_ref()
            && root == cwd
            && *completed >= requested
        {
            return Ok(commands.clone());
        }
        let commands = discover.await?;
        *latest = Some((cwd.to_owned(), std::time::Instant::now(), commands.clone()));
        Ok(commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_discovery_shares_overlap_but_refreshes_later_and_other_roots() {
        let discovery = CommandDiscovery::default();
        let probes = std::cell::Cell::new(0);
        let probe = || async {
            probes.set(probes.get() + 1);
            tokio::task::yield_now().await;
            Ok(vec![SlashCommand {
                name: format!("probe-{}", probes.get()),
                description: String::new(),
                input_hint: None,
            }])
        };
        let root = Path::new("/workspace");
        let (commands, skills) =
            tokio::join!(discovery.get(root, probe()), discovery.get(root, probe()),);
        assert_eq!(commands.unwrap()[0].name, skills.unwrap()[0].name);
        assert_eq!(
            probes.get(),
            1,
            "overlapping callers share one provider process"
        );
        assert_eq!(
            discovery.get(root, probe()).await.unwrap()[0].name,
            "probe-2"
        );
        let (a, b) = tokio::join!(
            discovery.get(root, probe()),
            discovery.get(Path::new("/other-workspace"), probe()),
        );
        assert_ne!(a.unwrap()[0].name, b.unwrap()[0].name);
        assert_eq!(probes.get(), 4);
        assert!(
            discovery
                .get(root, async { Err(HarnessError::Protocol("retry".into())) })
                .await
                .is_err()
        );
        assert_eq!(
            discovery.get(root, probe()).await.unwrap()[0].name,
            "probe-5"
        );
    }
}
