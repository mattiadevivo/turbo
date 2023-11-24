use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};
use turborepo_repository::package_graph::{WorkspaceName, ROOT_PKG_NAME};

pub const TASK_DELIMITER: &str = "#";

/// A task name as it appears in in a `turbo.json` it might be for all
/// workspaces or just one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct TaskName<'a> {
    package: Option<Cow<'a, str>>,
    task: Cow<'a, str>,
}

#[derive(Debug, thiserror::Error)]
#[error("No workspace found in task id '{input}'")]
pub struct TaskIdError<'a> {
    input: &'a str,
}

impl<'a> From<&'a str> for TaskName<'a> {
    fn from(value: &'a str) -> Self {
        match value.split_once(TASK_DELIMITER) {
            // Note we allow empty workspaces
            // In the future we shouldn't allow this and throw when we encounter them
            Some((package, task)) => Self {
                package: Some(package.into()),
                task: task.into(),
            },
            None => Self {
                package: None,
                task: value.into(),
            },
        }
    }
}

// Utility method changing the lifetime of an owned cow to reflect that it is
// owned
fn static_cow<'a, T: 'a + ToOwned + ?Sized>(cow: Cow<'a, T>) -> Cow<'static, T> {
    match cow {
        Cow::Borrowed(x) => Cow::Owned(x.to_owned()),
        Cow::Owned(x) => Cow::Owned(x),
    }
}

impl From<String> for TaskName<'static> {
    fn from(value: String) -> Self {
        let str = value.as_str();
        let TaskName { package, task } = TaskName::from(str);
        let package = package.map(static_cow);
        let task = static_cow(task);
        Self { package, task }
    }
}

impl<'a> fmt::Display for TaskName<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.package {
            Some(package) => f.write_fmt(format_args!("{package}{TASK_DELIMITER}{}", self.task)),
            None => f.write_str(&self.task),
        }
    }
}

impl<'a> From<TaskName<'a>> for String {
    fn from(value: TaskName<'a>) -> Self {
        value.to_string()
    }
}

impl<'a> TaskName<'a> {
    pub fn package(&self) -> Option<&str> {
        let package: &str = self.package.as_ref()?;
        Some(package)
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn into_non_workspace_task(self) -> Self {
        let Self { task, .. } = self;
        Self {
            package: None,
            task,
        }
    }

    // Makes a task a root workspace task
    // e.g. build to //#build
    pub fn into_root_task(self) -> TaskName<'static> {
        let Self { task, .. } = self;
        TaskName {
            package: Some(ROOT_PKG_NAME.into()),
            task: static_cow(task),
        }
    }

    pub fn task_id(&self) -> Option<TaskId<'_>> {
        let package: &str = self.package.as_deref()?;
        let task: &str = &self.task;
        Some(TaskId {
            package: package.into(),
            task: task.into(),
        })
    }

    pub fn is_package_task(&self) -> bool {
        self.package.is_some()
    }

    pub fn in_workspace(&self, workspace: &str) -> bool {
        self.task_id()
            .map_or(true, |task_id| task_id.package() == workspace)
    }

    pub fn into_owned(self) -> TaskName<'static> {
        let TaskName { package, task } = self;
        TaskName {
            package: package.map(static_cow),
            task: static_cow(task),
        }
    }
}

#[cfg(test)]
mod test {
    use test_case::test_case;

    use super::*;

    #[test_case("foo#build" ; "workspace task")]
    #[test_case("//#root" ; "root task")]
    #[test_case("@scope/foo#build" ; "workspace with scope")]
    fn test_roundtrip(input: &str) {
        assert_eq!(input, TaskId::try_from(input).unwrap().to_string());
    }

    #[test_case("foo", "build", "foo#build" ; "normal task")]
    #[test_case("foo", "bar#build", "bar#build" ; "workspace specific task")]
    #[test_case("foo", "//#build", "//#build" ; "root task")]
    fn test_new_task_id(package_name: &str, task_name: &str, expected: &str) {
        let expected = TaskId::try_from(expected).unwrap();
        let actual = TaskId::new(package_name, task_name);
        assert_eq!(actual, expected);
    }

    #[test_case("build" ; "global task")]
    #[test_case("foo#build" ; "workspace task")]
    #[test_case("//#build" ; "root task")]
    #[test_case("#build" ; "empty workspace")]
    fn test_task_name_roundtrip(input: &str) {
        assert_eq!(input, TaskName::from(input).to_string());
    }
}
