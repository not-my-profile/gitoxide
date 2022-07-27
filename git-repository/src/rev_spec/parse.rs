use crate::bstr::BStr;
use crate::types::RevSpecDetached;
use crate::RevSpec;
use crate::{object, Repository};
use git_hash::ObjectId;
use git_revision::spec::parse;
use git_revision::spec::parse::delegate::{self, PeelTo, ReflogLookup, SiblingBranch, Traversal};
use smallvec::SmallVec;
use std::collections::HashSet;

/// The error returned by [`crate::Repository::rev_parse()`].
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum Error {
    #[error(
    "The short hash {prefix} matched both the reference {} and at least one object", reference.name)]
    AmbiguousRefAndObject {
        /// The prefix to look for.
        prefix: git_hash::Prefix,
        /// The reference matching the prefix.
        reference: git_ref::Reference,
    },
    #[error(transparent)]
    IdFromHex(#[from] git_hash::decode::Error),
    #[error(transparent)]
    FindReference(#[from] git_ref::file::find::existing::Error),
    #[error(transparent)]
    FindObject(#[from] object::find::existing::OdbError),
    #[error(transparent)]
    PeelToKind(#[from] object::peel::to_kind::Error),
    #[error("Object {oid} was a {actual}, but needed it to be a {expected}")]
    ObjectKind {
        oid: ObjectId,
        actual: git_object::Kind,
        expected: git_object::Kind,
    },
    #[error(transparent)]
    Parse(#[from] parse::Error),
    #[error("An object prefixed {prefix} could not be found")]
    PrefixNotFound { prefix: git_hash::Prefix },
    #[error("Found the following objects prefixed with {prefix}: {}", info.iter().map(|(oid, info)| format!("\t{oid} {info}")).collect::<Vec<_>>().join("\t"))]
    AmbiguousPrefix {
        prefix: git_hash::Prefix,
        info: Vec<(ObjectId, error::CandidateInfo)>,
    },
    #[error("{current}")]
    Multi {
        current: Box<dyn std::error::Error + Send + Sync + 'static>,
        #[source]
        next: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
}

///
pub mod error {
    use super::Error;
    use crate::bstr::BString;
    use crate::Repository;
    use git_hash::ObjectId;
    use std::collections::HashSet;

    /// Additional information about candidates that caused ambiguity.
    #[derive(Debug)]
    pub enum CandidateInfo {
        /// An error occurred when looking up the object in the database.
        FindError {
            /// The reported error.
            source: crate::object::find::existing::OdbError,
        },
        /// The candidate is an object of the given `kind`.
        Object {
            /// The kind of the object.
            kind: git_object::Kind,
        },
        /// The candidate is a tag.
        Tag {
            /// The name of the tag.
            name: BString,
        },
        /// The candidate is a commit.
        Commit {
            /// The date of the commit.
            date: git_date::Time,
            /// The subject line.
            subject: BString,
        },
    }

    impl std::fmt::Display for CandidateInfo {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            todo!()
        }
    }

    impl Error {
        pub(crate) fn ambiguous(candidates: HashSet<ObjectId>, prefix: git_hash::Prefix, repo: &Repository) -> Self {
            Error::AmbiguousPrefix {
                prefix,
                info: Vec::new(),
            }
        }

        pub(crate) fn from_errors(errors: Vec<Self>) -> Self {
            assert!(!errors.is_empty());
            match errors.len() {
                0 => unreachable!(
                    "BUG: cannot create something from nothing, must have recorded some errors to call from_errors()"
                ),
                1 => errors.into_iter().next().expect("one"),
                _ => {
                    let mut it = errors.into_iter().rev();
                    let mut recent = Error::Multi {
                        current: Box::new(it.next().expect("at least one error")),
                        next: None,
                    };
                    for err in it {
                        recent = Error::Multi {
                            current: Box::new(err),
                            next: Some(Box::new(recent)),
                        }
                    }
                    recent
                }
            }
        }
    }
}

/// A hint to know what to do if refs and object names are equal.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RefsHint {
    /// This is the default, and leads to specs that look like objects identified by full hex sha and are objets to be used
    /// instead of similarly named references. The latter is not typical but can absolutely happen by accident.
    /// If the object prefix is shorter than the maximum hash length of the repository, use the reference instead, which is
    /// preferred as there are many valid object names like `beef` and `cafe` that are short and both valid and typical prefixes
    /// for objects.
    /// Git chooses this as default as well, even though it means that every object prefix is also looked up as ref.
    PreferObjectOnFullLengthHexShaUseRefOtherwise,
    /// No matter what, if it looks like an object prefix and has an object, use it.
    /// Note that no ref-lookup is made here which is the fastest option.
    PreferObject,
    /// When an object is found for a given prefix, also check if a reference exists with that name and if it does,
    /// use that moving forward.
    PreferRef,
    /// If there is an ambiguous situation, instead of silently choosing one over the other, fail instead.
    Fail,
}

/// A hint to know which object kind to prefer if multiple objects match a prefix.
///
/// This disambiguation mechanism is applied only if there is no disambiguation hints in the spec itself.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ObjectKindHint {
    /// Pick objects that are commits themselves.
    Commit,
    /// Pick objects that can be peeled into a commit, i.e. commits themselves or tags which are peeled until a commit is found.
    Committish,
    /// Pick objects that are trees themselves.
    Tree,
    /// Pick objects that can be peeled into a tree, i.e. trees themselves or tags which are peeled until a tree is found or commits
    /// whose tree is chosen.
    Treeish,
    /// Pick objects that are blobs.
    Blob,
}

impl Default for RefsHint {
    fn default() -> Self {
        RefsHint::PreferObjectOnFullLengthHexShaUseRefOtherwise
    }
}

/// Options for use in [`RevSpec::from_bstr()`].
#[derive(Debug, Default, Copy, Clone)]
pub struct Options {
    /// What to do if both refs and object names match the same input.
    pub refs_hint: RefsHint,
    /// The hint to use when encountering multiple object matching a prefix.
    ///
    /// If `None`, the rev-spec itself must disambiguate the object by drilling down to desired kinds or applying
    /// other disambiguating transformations.
    pub object_kind_hint: Option<ObjectKindHint>,
}

impl<'repo> RevSpec<'repo> {
    /// Parse `spec` and use information from `repo` to resolve it, using `opts` to learn how to deal with ambiguity.
    pub fn from_bstr<'a>(spec: impl Into<&'a BStr>, repo: &'repo Repository, opts: Options) -> Result<Self, Error> {
        fn zero_or_one_objects_or_ambguity_err(
            mut candidates: [Option<HashSet<ObjectId>>; 2],
            prefix: [Option<git_hash::Prefix>; 2],
            mut errors: Vec<Error>,
            repo: &Repository,
        ) -> Result<[Option<ObjectId>; 2], Error> {
            let mut out = [None, None];
            for ((candidates, prefix), out) in candidates.iter_mut().zip(prefix).zip(out.iter_mut()) {
                let candidates = candidates.take();
                match candidates {
                    None => *out = None,
                    Some(candidates) => {
                        match candidates.len() {
                            0 => unreachable!(
                                "BUG: let's avoid still being around if no candidate matched the requirements"
                            ),
                            1 => {
                                *out = candidates.into_iter().next();
                            }
                            _ => {
                                errors.insert(
                                    0,
                                    Error::ambiguous(candidates, prefix.expect("set when obtaining candidates"), repo),
                                );
                                return Err(Error::from_errors(errors));
                            }
                        };
                    }
                };
            }
            Ok(out)
        }
        let mut delegate = Delegate {
            refs: Default::default(),
            objs: Default::default(),
            idx: 0,
            kind: None,
            err: Vec::new(),
            prefix: Default::default(),
            last_call_was_disambiguate_prefix: Default::default(),
            opts,
            repo,
        };
        let spec = match git_revision::spec::parse(spec.into(), &mut delegate) {
            Err(parse::Error::Delegate) => {
                return Err(Error::from_errors(delegate.err));
            }
            Err(err) => return Err(err.into()),
            Ok(()) => {
                let range = zero_or_one_objects_or_ambguity_err(delegate.objs, delegate.prefix, delegate.err, repo)?;
                RevSpec {
                    inner: RevSpecDetached {
                        from_ref: delegate.refs[0].take(),
                        from: range[0],
                        to_ref: delegate.refs[1].take(),
                        to: range[1],
                        kind: delegate.kind,
                    },
                    repo,
                }
            }
        };
        Ok(spec)
    }
}

#[allow(dead_code)]
struct Delegate<'repo> {
    refs: [Option<git_ref::Reference>; 2],
    objs: [Option<HashSet<ObjectId>>; 2],
    idx: usize,
    kind: Option<git_revision::spec::Kind>,

    opts: Options,
    err: Vec<Error>,
    /// The ambiguous prefix obtained during a call to `disambiguate_prefix()`.
    prefix: [Option<git_hash::Prefix>; 2],
    /// If true, we didn't try to do any other transformation which might have helped with disambiguation.
    last_call_was_disambiguate_prefix: [bool; 2],

    repo: &'repo Repository,
}

impl<'repo> parse::Delegate for Delegate<'repo> {
    fn done(&mut self) {
        self.follow_refs_to_objects_if_needed();
        self.disambiguate_objects_by_fallback_hint();
    }
}

impl<'repo> Delegate<'repo> {
    fn disambiguate_objects_by_fallback_hint(&mut self) {
        if self.last_call_was_disambiguate_prefix[self.idx] {
            self.unset_disambiguate_call();

            if let Some(objs) = self.objs[self.idx].as_mut() {
                let repo = self.repo;
                let errors: Vec<_> = match self.opts.object_kind_hint {
                    Some(kind_hint) => match kind_hint {
                        ObjectKindHint::Treeish | ObjectKindHint::Committish => {
                            let kind = match kind_hint {
                                ObjectKindHint::Treeish => git_object::Kind::Tree,
                                ObjectKindHint::Committish => git_object::Kind::Commit,
                                _ => unreachable!("BUG: we narrow possibilities above"),
                            };
                            objs.iter()
                                .filter_map(|obj| peel(repo, obj, kind).err().map(|err| (*obj, err)))
                                .collect()
                        }
                        ObjectKindHint::Tree | ObjectKindHint::Commit | ObjectKindHint::Blob => {
                            let kind = match kind_hint {
                                ObjectKindHint::Tree => git_object::Kind::Tree,
                                ObjectKindHint::Commit => git_object::Kind::Commit,
                                ObjectKindHint::Blob => git_object::Kind::Blob,
                                _ => unreachable!("BUG: we narrow possibilities above"),
                            };
                            objs.iter()
                                .filter_map(|obj| require_object_kind(repo, obj, kind).err().map(|err| (*obj, err)))
                                .collect()
                        }
                    },
                    None => return,
                };

                if errors.len() == objs.len() {
                    self.err.extend(errors.into_iter().map(|(_, err)| err));
                } else {
                    for (obj, err) in errors {
                        objs.remove(&obj);
                        self.err.push(err);
                    }
                }
            }
        }
    }
    fn follow_refs_to_objects_if_needed(&mut self) -> Option<()> {
        assert_eq!(self.refs.len(), self.objs.len());
        for (r, obj) in self.refs.iter().zip(self.objs.iter_mut()) {
            if let (_ref_opt @ Some(ref_), obj_opt @ None) = (r, obj) {
                match ref_.target.try_id() {
                    Some(id) => obj_opt.get_or_insert_with(HashSet::default).insert(id.into()),
                    None => todo!("follow ref to get direct target object"),
                };
            };
        }
        Some(())
    }

    fn unset_disambiguate_call(&mut self) {
        self.last_call_was_disambiguate_prefix[self.idx] = false;
    }
}

impl<'repo> delegate::Revision for Delegate<'repo> {
    fn find_ref(&mut self, name: &BStr) -> Option<()> {
        self.unset_disambiguate_call();
        if !self.err.is_empty() && self.refs[self.idx].is_some() {
            return None;
        }
        match self.repo.refs.find(name) {
            Ok(r) => {
                assert!(self.refs[self.idx].is_none(), "BUG: cannot set the same ref twice");
                self.refs[self.idx] = Some(r);
                Some(())
            }
            Err(err) => {
                self.err.push(err.into());
                None
            }
        }
    }

    fn disambiguate_prefix(
        &mut self,
        prefix: git_hash::Prefix,
        _must_be_commit: Option<delegate::PrefixHint<'_>>,
    ) -> Option<()> {
        self.last_call_was_disambiguate_prefix[self.idx] = true;
        let mut candidates = Some(HashSet::default());
        self.prefix[self.idx] = Some(prefix);
        match self.repo.objects.lookup_prefix(prefix, candidates.as_mut()) {
            Err(err) => {
                self.err.push(object::find::existing::OdbError::Find(err).into());
                None
            }
            Ok(None) => {
                self.err.push(Error::PrefixNotFound { prefix });
                None
            }
            Ok(Some(Ok(_) | Err(()))) => {
                assert!(self.objs[self.idx].is_none(), "BUG: cannot set the same prefix twice");
                let candidates = candidates.expect("set above");
                match self.opts.refs_hint {
                    RefsHint::PreferObjectOnFullLengthHexShaUseRefOtherwise
                        if prefix.hex_len() == candidates.iter().next().expect("at least one").kind().len_in_hex() =>
                    {
                        self.objs[self.idx] = Some(candidates);
                        Some(())
                    }
                    RefsHint::PreferObject => {
                        self.objs[self.idx] = Some(candidates);
                        Some(())
                    }
                    RefsHint::PreferRef | RefsHint::PreferObjectOnFullLengthHexShaUseRefOtherwise | RefsHint::Fail => {
                        match self.repo.refs.find(&prefix.to_string()) {
                            Ok(ref_) => {
                                assert!(self.refs[self.idx].is_none(), "BUG: cannot set the same ref twice");
                                if self.opts.refs_hint == RefsHint::Fail {
                                    self.refs[self.idx] = Some(ref_.clone());
                                    self.err.push(Error::AmbiguousRefAndObject {
                                        prefix,
                                        reference: ref_,
                                    });
                                    self.err.push(Error::ambiguous(candidates, prefix, self.repo));
                                    None
                                } else {
                                    self.refs[self.idx] = Some(ref_);
                                    Some(())
                                }
                            }
                            Err(_) => {
                                self.objs[self.idx] = Some(candidates);
                                Some(())
                            }
                        }
                    }
                }
            }
        }
    }

    fn reflog(&mut self, _query: ReflogLookup) -> Option<()> {
        self.unset_disambiguate_call();
        todo!()
    }

    fn nth_checked_out_branch(&mut self, _branch_no: usize) -> Option<()> {
        self.unset_disambiguate_call();
        todo!()
    }

    fn sibling_branch(&mut self, _kind: SiblingBranch) -> Option<()> {
        self.unset_disambiguate_call();
        todo!()
    }
}

impl<'repo> delegate::Navigate for Delegate<'repo> {
    fn traverse(&mut self, _kind: Traversal) -> Option<()> {
        self.unset_disambiguate_call();
        todo!()
    }

    fn peel_until(&mut self, kind: PeelTo<'_>) -> Option<()> {
        self.unset_disambiguate_call();
        self.follow_refs_to_objects_if_needed()?;

        let mut replacements = SmallVec::<[(ObjectId, ObjectId); 1]>::default();
        let mut errors = Vec::new();
        let objs = self.objs[self.idx].as_mut()?;

        match kind {
            PeelTo::ValidObject => {
                for obj in objs.iter() {
                    match self.repo.find_object(*obj) {
                        Ok(_) => {}
                        Err(err) => {
                            errors.push((*obj, err.into()));
                        }
                    };
                }
            }
            PeelTo::ObjectKind(kind) => {
                let repo = self.repo;
                let peel = |obj| peel(repo, obj, kind);
                for obj in objs.iter() {
                    match peel(obj) {
                        Ok(replace) => replacements.push((*obj, replace)),
                        Err(err) => errors.push((*obj, err)),
                    }
                }
            }
            PeelTo::Path(_path) => todo!("lookup path"),
            PeelTo::RecursiveTagObject => todo!("recursive tag object"),
        }

        if errors.len() == objs.len() {
            self.err.extend(errors.into_iter().map(|(_, err)| err));
            None
        } else {
            for (obj, err) in errors {
                objs.remove(&obj);
                self.err.push(err);
            }
            for (find, replace) in replacements {
                objs.remove(&find);
                objs.insert(replace);
            }
            Some(())
        }
    }

    fn find(&mut self, _regex: &BStr, _negated: bool) -> Option<()> {
        self.unset_disambiguate_call();
        todo!()
    }

    fn index_lookup(&mut self, _path: &BStr, _stage: u8) -> Option<()> {
        self.unset_disambiguate_call();
        todo!()
    }
}

impl<'repo> delegate::Kind for Delegate<'repo> {
    fn kind(&mut self, _kind: git_revision::spec::Kind) -> Option<()> {
        todo!("kind, deal with ^ and .. and ... correctly")
    }
}

fn peel(repo: &Repository, obj: &git_hash::oid, kind: git_object::Kind) -> Result<ObjectId, Error> {
    let mut obj = repo.find_object(obj)?;
    obj = obj.peel_to_kind(kind)?;
    debug_assert_eq!(obj.kind, kind, "bug in Object::peel_to_kind() which didn't deliver");
    Ok(obj.id)
}

fn require_object_kind(repo: &Repository, obj: &git_hash::oid, kind: git_object::Kind) -> Result<(), Error> {
    let obj = repo.find_object(obj)?;
    if obj.kind == kind {
        Ok(())
    } else {
        Err(Error::ObjectKind {
            actual: obj.kind,
            expected: kind,
            oid: obj.id,
        })
    }
}
