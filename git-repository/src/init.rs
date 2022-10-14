use crate::bstr::BString;
use git_ref::store::WriteReflog;
use git_ref::transaction::{PreviousValue, RefEdit};
use git_ref::{FullName, Target};
use std::borrow::Cow;
use std::convert::TryInto;
use std::path::Path;

use crate::ThreadSafeRepository;

/// The name of the branch to use if non is configured via git configuration.
///
/// # Deviation
///
/// We use `main` instead of `master`.
pub const DEFAULT_BRANCH_NAME: &str = "main";

/// The error returned by [`crate::init()`].
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum Error {
    #[error(transparent)]
    Init(#[from] crate::create::Error),
    #[error(transparent)]
    Open(#[from] crate::open::Error),
    #[error("Invalid default branch name: {name:?}")]
    InvalidBranchName {
        name: BString,
        source: git_validate::refname::Error,
    },
    #[error("Could not edit HEAD reference with new default name")]
    EditHeadForDefaultBranch(#[from] crate::reference::edit::Error),
}

impl ThreadSafeRepository {
    /// Create a repository with work-tree within `directory`, creating intermediate directories as needed.
    ///
    /// Fails without action if there is already a `.git` repository inside of `directory`, but
    /// won't mind if the `directory` otherwise is non-empty.
    pub fn init(directory: impl AsRef<Path>, options: crate::create::Options) -> Result<Self, Error> {
        use git_sec::trust::DefaultForLevel;
        let open_options = crate::open::Options::default_for_level(git_sec::Trust::Full);
        Self::init_opts(directory, options, open_options)
    }

    /// Similar to [`init`][Self::init()], but allows to determine how exactly to open the newly created repository.
    ///
    /// # Deviation
    ///
    /// Instead of naming the default branch `master`, we name it `main` unless configured explicitly using the `init.defaultBranch`
    /// configuration key.
    pub fn init_opts(
        directory: impl AsRef<Path>,
        create_options: crate::create::Options,
        mut open_options: crate::open::Options,
    ) -> Result<Self, Error> {
        let path = crate::create::into(directory.as_ref(), create_options)?;
        let (git_dir, worktree_dir) = path.into_repository_and_work_tree_directories();
        open_options.git_dir_trust = Some(git_sec::Trust::Full);
        let repo = ThreadSafeRepository::open_from_paths(git_dir, worktree_dir, open_options)?;

        let branch_name = repo
            .config
            .resolved
            .string("init", None, "defaultBranch")
            .unwrap_or_else(|| Cow::Borrowed(DEFAULT_BRANCH_NAME.into()));
        if branch_name.as_ref() != DEFAULT_BRANCH_NAME {
            let sym_ref: FullName =
                format!("refs/heads/{branch_name}")
                    .try_into()
                    .map_err(|err| Error::InvalidBranchName {
                        name: branch_name.into_owned(),
                        source: err,
                    })?;
            let mut repo = repo.to_thread_local();
            repo.refs.write_reflog = WriteReflog::Disable;
            repo.edit_reference(RefEdit {
                change: git_ref::transaction::Change::Update {
                    log: Default::default(),
                    expected: PreviousValue::Any,
                    new: Target::Symbolic(sym_ref),
                },
                name: "HEAD".try_into().expect("valid"),
                deref: false,
            })?;
        }

        Ok(repo)
    }
}
