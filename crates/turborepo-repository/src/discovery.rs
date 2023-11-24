//! turborepo-discovery
//!
//! This package contains a number of strategies for discovering various things
//! about a workspace. These traits come with a basic implementation and some
//! adaptors that can be used to compose them together.
//!
//! This powers various intents such as 'query the daemon for this data, or
//! fallback to local discovery if the daemon is not available'. Eventually,
//! these strategies will implement some sort of monad-style composition so that
//! we can track areas of run that are performing sub-optimally.

use std::{borrow::Cow, collections::HashMap, fmt};

use tokio_stream::{iter, StreamExt};
use turbopath::AbsoluteSystemPathBuf;

use crate::{
    package_graph,
    package_json::PackageJson,
    package_manager::{self, PackageManager},
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkspaceData {
    pub package_json: AbsoluteSystemPathBuf,
    pub turbo_json: Option<AbsoluteSystemPathBuf>,
}

#[derive(Debug, Clone)]
pub struct DiscoveryResponse {
    pub workspaces: Vec<WorkspaceData>,
    pub package_manager: PackageManager,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("discovery unavailable")]
    Unavailable,
    #[error("discovery failed: {0}")]
    Failed(Box<dyn std::error::Error + Send + Sync>),
}

/// Defines a strategy for discovering packages on the filesystem.
pub trait PackageDiscovery {
    // desugar to assert that the future is Send
    fn discover_packages(
        &mut self,
    ) -> impl std::future::Future<Output = Result<DiscoveryResponse, Error>> + Send;
}

/// We want to allow for lazily generating the PackageDiscovery implementation
/// to prevent unnecessary work. This trait allows us to do that.
///
/// Note: there is a blanket implementation for everything that implements
/// PackageDiscovery
pub trait PackageDiscoveryBuilder {
    type Output: PackageDiscovery;
    type Error: std::error::Error;

    fn build(self) -> Result<Self::Output, Self::Error>;
}

impl<T: PackageDiscovery + Send> PackageDiscovery for Option<T> {
    async fn discover_packages(&mut self) -> Result<DiscoveryResponse, Error> {
        match self {
            Some(d) => d.discover_packages().await,
            None => Err(Error::Unavailable),
        }
    }
}

pub struct LocalPackageDiscovery {
    repo_root: AbsoluteSystemPathBuf,
    package_manager: PackageManager,
}

impl LocalPackageDiscovery {
    pub fn new(repo_root: AbsoluteSystemPathBuf, package_manager: PackageManager) -> Self {
        Self {
            repo_root,
            package_manager,
        }
    }
}

pub struct LocalPackageDiscoveryBuilder {
    repo_root: AbsoluteSystemPathBuf,
    package_manager: Option<PackageManager>,
    package_json: Option<PackageJson>,
}

impl LocalPackageDiscoveryBuilder {
    pub fn new(
        repo_root: AbsoluteSystemPathBuf,
        package_manager: Option<PackageManager>,
        package_json: Option<PackageJson>,
    ) -> Self {
        Self {
            repo_root,
            package_manager,
            package_json,
        }
    }
}

impl PackageDiscoveryBuilder for LocalPackageDiscoveryBuilder {
    type Output = LocalPackageDiscovery;
    type Error = package_manager::Error;

    fn build(self) -> Result<Self::Output, Self::Error> {
        let package_manager = match self.package_manager {
            Some(pm) => pm,
            None => {
                PackageManager::get_package_manager(&self.repo_root, self.package_json.as_ref())?
            }
        };

        Ok(LocalPackageDiscovery {
            repo_root: self.repo_root,
            package_manager,
        })
    }
}

impl PackageDiscovery for LocalPackageDiscovery {
    async fn discover_packages(&mut self) -> Result<DiscoveryResponse, Error> {
        let packages = match self.package_manager.get_package_jsons(&self.repo_root) {
            Ok(packages) => packages,
            // if there is not a list of workspaces, it is not necessarily an error. just report no
            // workspaces
            Err(package_manager::Error::Workspace(_)) => {
                return Ok(DiscoveryResponse {
                    workspaces: vec![],
                    package_manager: self.package_manager,
                })
            }
            Err(e) => return Err(Error::Failed(Box::new(e))),
        };

        iter(packages)
            .then(|a| async move {
                let potential_turbo = a.parent().expect("non-root").join_component("turbo.json");
                let potential_turbo_exists = tokio::fs::try_exists(potential_turbo.as_path()).await;

                Ok(WorkspaceData {
                    package_json: a,
                    turbo_json: potential_turbo_exists
                        .ok()
                        .and_then(|pe| pe.then_some(potential_turbo)),
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .await
            .map(|workspaces| DiscoveryResponse {
                workspaces,
                package_manager: self.package_manager,
            })
    }
}

/// Attempts to run the `primary` strategy for an amount of time
/// specified by `timeout` before falling back to `fallback`
pub struct FallbackPackageDiscovery<P, F> {
    primary: P,
    fallback: F,
    timeout: std::time::Duration,
}

impl<P: PackageDiscovery, F: PackageDiscovery> FallbackPackageDiscovery<P, F> {
    pub fn new(primary: P, fallback: F, timeout: std::time::Duration) -> Self {
        Self {
            primary,
            fallback,
            timeout,
        }
    }
}

impl<T: PackageDiscovery> PackageDiscoveryBuilder for T {
    type Output = T;
    type Error = std::convert::Infallible;

    fn build(self) -> Result<Self::Output, Self::Error> {
        Ok(self)
    }
}

impl<A: PackageDiscovery + Send, B: PackageDiscovery + Send> PackageDiscovery
    for FallbackPackageDiscovery<A, B>
{
    async fn discover_packages(&mut self) -> Result<DiscoveryResponse, Error> {
        match tokio::time::timeout(self.timeout, self.primary.discover_packages()).await {
            Ok(Ok(packages)) => Ok(packages),
            Ok(Err(err1)) => match self.fallback.discover_packages().await {
                Ok(packages) => Ok(packages),
                // if the backup is unavailable, return the original error
                Err(Error::Unavailable) => Err(err1),
                Err(err2) => Err(err2),
            },
            Err(_) => self.fallback.discover_packages().await,
        }
    }
}

pub struct CachingPackageDiscovery<P: PackageDiscovery> {
    primary: P,
    data: Option<DiscoveryResponse>,
}

impl<P: PackageDiscovery> CachingPackageDiscovery<P> {
    pub fn new(primary: P) -> Self {
        Self {
            primary,
            data: None,
        }
    }
}

impl<P: PackageDiscovery + Send> PackageDiscovery for CachingPackageDiscovery<P> {
    async fn discover_packages(&mut self) -> Result<DiscoveryResponse, Error> {
        match self.data.take() {
            Some(data) => Ok(data),
            None => {
                let data = self.primary.discover_packages().await?;
                self.data = Some(data.clone());
                Ok(data)
            }
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum DiscoveryError {
    #[error("discovery unavailable")]
    Unavailable,
    #[error("discovery failed: {0}")]
    Failed(Box<dyn std::error::Error + Send + Sync>),
}

pub trait HashDiscovery {
    fn discover_hashes(
        &mut self,
    ) -> impl std::future::Future<Output = Result<HashDiscoveryResponse, DiscoveryError>>;
}

pub struct HashDiscoveryResponse {
    pub hashes: PackageInputsHashes,
}

pub struct LocalHashDiscovery<'a> {
    repo_root: AbsoluteSystemPathBuf,
    workspaces: HashMap<&'a package_graph::WorkspaceName, &'a package_graph::WorkspaceInfo>,
}

impl<'a> LocalHashDiscovery<'a> {
    pub fn new(
        repo_root: AbsoluteSystemPathBuf,
        workspaces: HashMap<&'a package_graph::WorkspaceName, &'a package_graph::WorkspaceInfo>,
    ) -> Self {
        Self {
            repo_root,
            workspaces,
        }
    }
}

impl<'a> HashDiscovery for LocalHashDiscovery<'a> {
    async fn discover_hashes(&mut self) -> Result<HashDiscoveryResponse, DiscoveryError> {
        todo!()
    }
}

#[derive(Debug, Default)]
pub struct PackageInputsHashes {
    pub hashes: HashMap<TaskId<'static>, String>,
    pub expanded_hashes: HashMap<TaskId<'static>, FileHashes>,
}

use serde::Serialize;

/// A task identifier as it will appear in the task graph
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(from = "String", into = "String")]
pub struct TaskId<'a> {
    package: Cow<'a, str>,
    task: Cow<'a, str>,
}

impl<'a> From<TaskId<'a>> for String {
    fn from(value: TaskId<'a>) -> Self {
        value.to_string()
    }
}

pub const TASK_DELIMITER: &str = "#";

impl<'a> fmt::Display for TaskId<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "{}{TASK_DELIMITER}{}",
            self.package, self.task
        ))
    }
}

impl<'a> TaskId<'a> {
    pub fn new(package: &'a str, task: &'a str) -> Self {
        TaskId::try_from(task).unwrap_or_else(|_| Self {
            package: package.into(),
            task: task.into(),
        })
    }

    pub fn from_graph(workspace: &WorkspaceName, task_name: &TaskName) -> TaskId<'static> {
        task_name.task_id().map_or_else(
            || {
                let package = match workspace {
                    WorkspaceName::Root => ROOT_PKG_NAME.into(),
                    WorkspaceName::Other(workspace) => static_cow(workspace.as_str().into()),
                };
                TaskId {
                    package,
                    task: static_cow(task_name.task().into()),
                }
            },
            |id| id.into_owned(),
        )
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn to_workspace_name(&self) -> WorkspaceName {
        match self.package.as_ref() {
            ROOT_PKG_NAME => WorkspaceName::Root,
            package => WorkspaceName::Other(package.into()),
        }
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn as_non_workspace_task_name(&self) -> TaskName {
        let task: &str = &self.task;
        TaskName {
            package: None,
            task: task.into(),
        }
    }

    pub fn as_task_name(&self) -> TaskName {
        let package: &str = &self.package;
        let task: &str = &self.task;
        TaskName {
            package: Some(package.into()),
            task: task.into(),
        }
    }

    pub fn into_owned(self) -> TaskId<'static> {
        let TaskId { package, task } = self;
        TaskId {
            package: static_cow(package),
            task: static_cow(task),
        }
    }
}

impl<'a> TryFrom<&'a str> for TaskId<'a> {
    type Error = TaskIdError<'a>;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        // We use split once here as the Go code will fail to find any task
        //  name that contains a '#' in the task graph.
        // e.g. workspace#test#check can't run as we'll look for test and
        // attempt to run test instead of test#check
        match value.split_once(TASK_DELIMITER) {
            None | Some(("", _)) => Err(TaskIdError { input: value }),
            Some((package, task)) => Ok(TaskId {
                package: package.into(),
                task: task.into(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileHashes(pub HashMap<turbopath::RelativeUnixPathBuf, String>);

#[cfg(test)]
mod fallback_tests {
    use std::time::Duration;

    use tokio::runtime::Runtime;

    use super::*;

    struct MockDiscovery {
        should_fail: bool,
        calls: usize,
    }

    impl MockDiscovery {
        fn new(should_fail: bool) -> Self {
            Self {
                should_fail,
                calls: 0,
            }
        }
    }

    impl PackageDiscovery for MockDiscovery {
        async fn discover_packages(&mut self) -> Result<DiscoveryResponse, Error> {
            if self.should_fail {
                Err(Error::Failed(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "mock error",
                ))))
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                self.calls += 1;
                // Simulate successful package discovery
                Ok(DiscoveryResponse {
                    package_manager: PackageManager::Npm,
                    workspaces: vec![],
                })
            }
        }
    }

    #[test]
    fn test_fallback_on_primary_failure() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let primary = MockDiscovery::new(true);
            let fallback = MockDiscovery::new(false);

            let mut discovery =
                FallbackPackageDiscovery::new(primary, fallback, Duration::from_secs(5));

            // Invoke the method under test
            let result = discovery.discover_packages().await;

            // Assert that the fallback was used and successful
            assert!(result.is_ok());

            // Assert that the fallback was used
            assert_eq!(discovery.primary.calls, 0);
            assert_eq!(discovery.fallback.calls, 1);
        });
    }

    #[test]
    fn test_fallback_on_primary_timeout() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let primary = MockDiscovery::new(false);
            let fallback = MockDiscovery::new(false);

            let mut discovery =
                FallbackPackageDiscovery::new(primary, fallback, Duration::from_secs(0));

            // Invoke the method under test
            let result = discovery.discover_packages().await;

            // Assert that the fallback was used and successful
            assert!(result.is_ok());

            // Assert that the fallback was used
            assert_eq!(discovery.primary.calls, 0);
            assert_eq!(discovery.fallback.calls, 1);
        });
    }
}

#[cfg(test)]
mod caching_tests {
    use tokio::runtime::Runtime;

    use super::*;

    struct MockPackageDiscovery {
        call_count: usize,
    }

    impl PackageDiscovery for MockPackageDiscovery {
        async fn discover_packages(&mut self) -> Result<DiscoveryResponse, Error> {
            self.call_count += 1;
            // Simulate successful package discovery
            Ok(DiscoveryResponse {
                package_manager: PackageManager::Npm,
                workspaces: vec![],
            })
        }
    }

    #[test]
    fn test_caching_package_discovery() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let primary = MockPackageDiscovery { call_count: 0 };
            let mut discovery = CachingPackageDiscovery::new(primary);

            // First call should use primary discovery
            let _first_result = discovery.discover_packages().await.unwrap();
            assert_eq!(discovery.primary.call_count, 1);

            // Second call should use cached data and not increase call count
            let _second_result = discovery.discover_packages().await.unwrap();
            assert_eq!(discovery.primary.call_count, 1);
        });
    }
}
