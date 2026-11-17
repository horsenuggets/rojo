//! Decides which subtrees syncback stores in a single model file.
//!
//! The `modelTrees` syncback rule matches Instances by their path in the
//! DataModel. A matching Instance and all of its descendants are written to one
//! `.rbxm` or `.rbxmx` file, instead of a file or directory for each of them.

use std::collections::{HashMap, HashSet};

use anyhow::Context;
use globset::{GlobBuilder, GlobMatcher};
use rbx_dom_weak::{types::Ref, WeakDom};
use serde::{Deserialize, Serialize};

use crate::{
    project::ProjectNode,
    snapshot::{InstanceMetadata, InstigatingSource, RojoTree},
    snapshot_middleware::Middleware,
};

/// The file format a `modelTrees` rule writes its matches in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTreeFormat {
    /// A binary model file, which is the smallest on disk.
    #[default]
    Rbxm,
    /// An XML model file, which is larger but can be read and diffed as text.
    Rbxmx,
}

impl ModelTreeFormat {
    fn middleware(self) -> Middleware {
        match self {
            Self::Rbxm => Middleware::Rbxm,
            Self::Rbxmx => Middleware::Rbxmx,
        }
    }
}

/// One entry of the `modelTrees` syncback rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelTreeRule {
    /// A glob matched against the path of an Instance in the DataModel, like
    /// `Workspace/Maps/*/Effects`. `*` and `?` stay within one Instance name,
    /// and `**` crosses any number of them.
    pattern: String,
    /// The format matching Instances are written in. Defaults to `rbxm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format: Option<ModelTreeFormat>,
}

/// The `modelTrees` rules of a project, ready to match Instance paths.
pub struct CompiledModelTreeRules {
    rules: Vec<(GlobMatcher, Middleware)>,
    /// Paths of the Instances the project file defines. The project fixes
    /// where and how each of them is stored, so rules never apply to them.
    project_nodes: HashSet<String>,
}

impl CompiledModelTreeRules {
    pub fn new(rules: &[ModelTreeRule], project_tree: &ProjectNode) -> anyhow::Result<Self> {
        let mut project_nodes = HashSet::new();
        if !rules.is_empty() {
            let mut pending = vec![(project_tree, String::new())];
            while let Some((node, path)) = pending.pop() {
                for (name, child) in &node.children {
                    let child_path = child_path(&path, name);
                    project_nodes.insert(child_path.clone());
                    pending.push((child, child_path));
                }
            }
        }

        let rules = rules
            .iter()
            .map(|rule| {
                let glob = GlobBuilder::new(&rule.pattern)
                    // Without this, `*` would also match nested Instances.
                    .literal_separator(true)
                    .build()
                    .with_context(|| {
                        format!("the pattern '{}' is not a valid glob", rule.pattern)
                    })?;
                Ok((
                    glob.compile_matcher(),
                    rule.format.unwrap_or_default().middleware(),
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            rules,
            project_nodes,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Returns the model middleware for the Instance at `inst_path`, taken
    /// from the first rule that matches it. Instances the project file defines
    /// never match, but their descendants can.
    pub fn middleware_for(&self, inst_path: &str) -> Option<Middleware> {
        if self.project_nodes.contains(inst_path) {
            return None;
        }
        self.rules
            .iter()
            .find(|(matcher, _)| matcher.is_match(inst_path))
            .map(|(_, middleware)| *middleware)
    }
}

/// The Instances of a DOM that `modelTrees` rules store as model files.
#[derive(Default)]
pub struct ModelTrees {
    roots: HashMap<Ref, Middleware>,
    containers: HashSet<Ref>,
}

impl ModelTrees {
    /// Finds every Instance in `dom` that matches a rule. Matches inside
    /// another match are found too. Syncback never reaches them when the
    /// outer one becomes a model file, but it does when the outer one keeps
    /// its layout because a project defines it.
    pub fn find(rules: &CompiledModelTreeRules, dom: &WeakDom) -> Self {
        let mut trees = Self::default();
        if rules.is_empty() {
            return trees;
        }
        // The root has an empty path, so it can't match and is never a model.
        let mut pending = vec![(dom.root_ref(), String::new())];
        while let Some((referent, path)) = pending.pop() {
            let instance = dom.get_by_ref(referent).unwrap();
            for child_ref in instance.children() {
                let child = dom.get_by_ref(*child_ref).unwrap();
                let child_path = child_path(&path, &child.name);
                if let Some(middleware) = rules.middleware_for(&child_path) {
                    trees.roots.insert(*child_ref, middleware);
                    let mut current = *child_ref;
                    while trees.containers.insert(current) {
                        match dom.get_by_ref(current) {
                            Some(ancestor) if ancestor.parent().is_some() => {
                                current = ancestor.parent()
                            }
                            _ => break,
                        }
                    }
                }
                pending.push((*child_ref, child_path));
            }
        }
        trees
    }

    /// Returns the model middleware a rule assigns to this Instance.
    pub fn middleware(&self, referent: Ref) -> Option<Middleware> {
        self.roots.get(&referent).copied()
    }

    /// Returns whether this Instance matches a rule or has a descendant that
    /// does.
    pub fn contains(&self, referent: Ref) -> bool {
        self.containers.contains(&referent)
    }
}

/// Returns whether a middleware stores a whole subtree in one binary or XML
/// model file.
pub fn is_model_file(middleware: Middleware) -> bool {
    matches!(middleware, Middleware::Rbxm | Middleware::Rbxmx)
}

/// Returns whether a project decides how this Instance is stored, either
/// through a node of a project file or by being the root of a nested one. The
/// `modelTrees` rules leave such Instances alone and apply below them.
pub fn is_project_owned(metadata: &InstanceMetadata) -> bool {
    metadata.middleware == Some(Middleware::Project)
        || matches!(
            metadata.instigating_source,
            Some(InstigatingSource::ProjectNode { .. })
        )
}

/// Returns every instance in `tree` whose subtree is on disk in a layout the
/// `modelTrees` rules no longer ask for. That is a matching Instance stored
/// any other way than its rule's format, or, when `as_directories` is set, a
/// model file no rule matches. Syncback skips subtrees whose content is
/// unchanged, so without this a changed rule would only apply to Instances
/// that happen to change in the place.
pub fn needs_relayout(
    rules: &CompiledModelTreeRules,
    tree: &RojoTree,
    as_directories: bool,
) -> HashSet<Ref> {
    let mut relayout = HashSet::new();
    for stale in stale_layouts(rules, tree, as_directories) {
        // Every ancestor's subtree includes the misplaced instance.
        let mut current = tree.get_instance(stale).unwrap().parent();
        while relayout.insert(current) {
            match tree.get_instance(current) {
                Some(ancestor) if ancestor.parent().is_some() => current = ancestor.parent(),
                _ => break,
            }
        }
    }
    relayout
}

/// Returns the instances whose own layout on disk the `modelTrees` rules no
/// longer ask for, as described for [`needs_relayout`]. None of them is inside
/// another, since a model file's descendants have no layout of their own.
pub fn stale_layouts(
    rules: &CompiledModelTreeRules,
    tree: &RojoTree,
    as_directories: bool,
) -> Vec<Ref> {
    let mut stale_instances = Vec::new();
    if rules.is_empty() && !as_directories {
        return stale_instances;
    }
    let dom = tree.inner();
    let mut pending = vec![(tree.get_root_id(), String::new())];
    while let Some((referent, path)) = pending.pop() {
        let instance = dom.get_by_ref(referent).unwrap();
        for child_ref in instance.children() {
            let child = dom.get_by_ref(*child_ref).unwrap();
            let child_path = child_path(&path, &child.name);
            let Some(metadata) = tree.get_metadata(*child_ref) else {
                continue;
            };
            // Project nodes fix both the path and the format of their source,
            // and a nested project file is never replaced by a model.
            if is_project_owned(metadata) {
                pending.push((*child_ref, child_path));
                continue;
            }
            let wanted = rules.middleware_for(&child_path);
            let stale = match (wanted, metadata.middleware) {
                (Some(wanted), current) => current != Some(wanted),
                (None, Some(current)) => as_directories && is_model_file(current),
                (None, None) => false,
            };
            if stale {
                stale_instances.push(*child_ref);
            }
            // A model file holds its descendants, which have no layout of
            // their own to check.
            let is_model = wanted.is_some() || metadata.middleware.is_some_and(is_model_file);
            if !is_model {
                pending.push((*child_ref, child_path));
            }
        }
    }
    stale_instances
}

fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_owned()
    } else {
        format!("{parent_path}/{name}")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::Path;

    use rbx_dom_weak::InstanceBuilder;

    use crate::{
        snapshot::{InstanceMetadata, InstanceSnapshot},
        syncback::inline,
    };

    fn rules(rules: &[(&str, Option<ModelTreeFormat>)]) -> CompiledModelTreeRules {
        let rules: Vec<_> = rules
            .iter()
            .map(|(pattern, format)| ModelTreeRule {
                pattern: pattern.to_string(),
                format: *format,
            })
            .collect();
        CompiledModelTreeRules::new(&rules, &ProjectNode::default()).unwrap()
    }

    /// Runs a full syncback of `input` over a project on disk whose `src`
    /// directory is the root Instance, then writes the result.
    fn syncback(root: &Path, syncback_rules: &str, input: WeakDom) -> anyhow::Result<()> {
        let project_path = root.join("default.project.json");
        std::fs::write(
            &project_path,
            format!(
                r#"{{"name":"Test","tree":{{"$path":"src"}},"syncbackRules":{syncback_rules}}}"#
            ),
        )?;
        let vfs = memofs::Vfs::new_default()?;
        vfs.set_watch_enabled(false);
        let session = crate::serve_session::ServeSession::new(vfs, &project_path)?;
        let snapshot = crate::syncback::syncback_loop(
            session.vfs(),
            &mut session.tree(),
            input,
            session.root_project(),
        )?;
        snapshot.write_to_vfs(root, session.vfs())?;
        Ok(())
    }

    /// A root folder holding `FX`, which holds a `Keep` folder.
    fn effects_dom() -> WeakDom {
        WeakDom::new(
            InstanceBuilder::new("Folder").with_child(
                InstanceBuilder::new("Folder")
                    .with_name("FX")
                    .with_child(InstanceBuilder::new("Folder").with_name("Keep")),
            ),
        )
    }

    fn project_on_disk() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        (temp, root)
    }

    #[test]
    fn conversions_never_remove_ignored_paths() {
        let (_temp, root) = project_on_disk();
        let keep = root.join("src/FX/Keep");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("Notes.luau"), "return nil\n").unwrap();

        let error = syncback(
            &root,
            r#"{
                "instancesAsDirectories": true,
                "ignorePaths": ["src/FX/Keep/**"],
                "modelTrees": [{ "pattern": "FX" }]
            }"#,
            effects_dom(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("ignorePaths"), "{error}");
        assert!(keep.join("Notes.luau").is_file());
        assert!(!root.join("src/FX.rbxm").exists());
    }

    #[test]
    fn conversions_never_remove_ignored_files_rojo_does_not_read() {
        let (_temp, root) = project_on_disk();
        let effects = root.join("src/FX");
        std::fs::create_dir_all(effects.join("Keep")).unwrap();
        std::fs::write(effects.join("notes.md"), "Remember this.\n").unwrap();

        let error = syncback(
            &root,
            r#"{
                "instancesAsDirectories": true,
                "ignorePaths": ["**/*.md"],
                "modelTrees": [{ "pattern": "FX" }]
            }"#,
            effects_dom(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("notes.md"), "{error}");
        assert!(effects.join("notes.md").is_file());
        assert!(!root.join("src/FX.rbxm").exists());
    }

    #[test]
    fn a_directory_named_like_the_model_file_makes_way_for_it() {
        let (_temp, root) = project_on_disk();
        let part = r#"{ "className": "Part" }"#;
        for holder in ["FX", "FX.rbxm"] {
            let child = root.join("src").join(holder).join("Spark");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(child.join("init.meta.json"), part).unwrap();
        }
        let holder = |name: &str| {
            InstanceBuilder::new("Folder")
                .with_name(name)
                .with_child(InstanceBuilder::new("Part").with_name("Spark"))
        };

        syncback(
            &root,
            r#"{
                "instancesAsDirectories": true,
                "modelTrees": [{ "pattern": "FX" }]
            }"#,
            WeakDom::new(
                InstanceBuilder::new("Folder")
                    .with_child(holder("FX"))
                    .with_child(holder("FX.rbxm")),
            ),
        )
        .unwrap();

        assert!(root.join("src/FX.rbxm").is_file());
        assert!(!root.join("src/FX").exists());
        let moved = std::fs::read_to_string(root.join("src/FX.rbxm~2/init.meta.json")).unwrap();
        assert!(moved.contains(r#""name": "FX.rbxm""#), "{moved}");
    }

    #[test]
    fn a_displaced_directory_still_protects_its_ignored_files() {
        let (_temp, root) = project_on_disk();
        let part = r#"{ "className": "Part" }"#;
        for holder in ["FX", "FX.rbxm"] {
            let child = root.join("src").join(holder).join("Spark");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(child.join("init.meta.json"), part).unwrap();
        }
        let notes = root.join("src/FX/notes.md");
        std::fs::write(&notes, "Remember this.\n").unwrap();
        let holder = |name: &str| {
            InstanceBuilder::new("Folder")
                .with_name(name)
                .with_child(InstanceBuilder::new("Part").with_name("Spark"))
        };

        // The model file takes the name the other directory would use, which
        // leaves that directory's own files unpaired and up for removal.
        let error = syncback(
            &root,
            r#"{
                "instancesAsDirectories": true,
                "ignorePaths": ["**/*.md"],
                "modelTrees": [{ "pattern": "FX" }]
            }"#,
            WeakDom::new(
                InstanceBuilder::new("Folder")
                    .with_child(holder("FX"))
                    .with_child(holder("FX.rbxm")),
            ),
        )
        .unwrap_err();

        assert!(error.to_string().contains("notes.md"), "{error}");
        assert!(notes.is_file());
        assert!(root.join("src/FX").is_dir());
        assert!(root.join("src/FX.rbxm").is_dir());
    }

    #[test]
    fn nested_project_files_are_never_replaced() {
        let (_temp, root) = project_on_disk();
        let nested = root.join("src/Nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("default.project.json"),
            r#"{"name":"Nested","tree":{"$className":"Folder"}}"#,
        )
        .unwrap();

        syncback(
            &root,
            r#"{ "modelTrees": [{ "pattern": "**" }] }"#,
            WeakDom::new(
                InstanceBuilder::new("Folder")
                    .with_child(InstanceBuilder::new("Folder").with_name("Nested"))
                    .with_child(InstanceBuilder::new("Folder").with_name("Other")),
            ),
        )
        .unwrap();

        assert!(nested.join("default.project.json").is_file());
        assert!(!root.join("src/Nested.rbxm").exists());
        assert!(root.join("src/Other.rbxm").is_file());
    }

    #[test]
    fn models_never_absorb_ignored_trees() {
        let (_temp, root) = project_on_disk();

        let error = syncback(
            &root,
            r#"{
                "ignoreTrees": ["FX/Keep"],
                "modelTrees": [{ "pattern": "FX" }]
            }"#,
            effects_dom(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("ignoreTrees"), "{error}");
        assert!(!root.join("src/FX.rbxm").exists());
    }

    #[test]
    fn rewriting_a_model_keeps_its_other_metadata() {
        let (_temp, root) = project_on_disk();
        let rules = r#"{
            "instancesAsDirectories": true,
            "modelTrees": [{ "pattern": "*", "format": "rbxmx" }]
        }"#;
        let input = |child: Option<&str>| {
            let mut model = InstanceBuilder::new("Folder").with_name("Model?");
            if let Some(child) = child {
                model = model.with_child(InstanceBuilder::new("Folder").with_name(child));
            }
            WeakDom::new(InstanceBuilder::new("Folder").with_child(model))
        };
        let json = root.join("src/Model_.meta.json");
        let jsonc = root.join("src/Model_.meta.jsonc");

        syncback(&root, rules, input(None)).unwrap();
        assert!(root.join("src/Model_.rbxmx").is_file());
        let written = std::fs::read_to_string(&json).unwrap();
        assert!(written.contains(r#""name": "Model?""#), "{written}");

        // The metadata moves to a JSONC file and gains a field of its own.
        std::fs::remove_file(&json).unwrap();
        std::fs::write(
            &jsonc,
            r#"{ "name": "Model?", "ignoreUnknownInstances": true }"#,
        )
        .unwrap();
        syncback(&root, rules, input(Some("Added"))).unwrap();

        let model = std::fs::read_to_string(root.join("src/Model_.rbxmx")).unwrap();
        assert!(model.contains("Added"), "{model}");
        let rewritten = std::fs::read_to_string(&jsonc).unwrap();
        assert!(rewritten.contains(r#""name": "Model?""#), "{rewritten}");
        assert!(rewritten.contains("ignoreUnknownInstances"), "{rewritten}");
        assert!(!json.exists());
    }

    #[test]
    fn project_nodes_never_match_but_their_descendants_do() {
        let mut source = ProjectNode::default();
        source
            .children
            .insert("Shared".to_owned(), ProjectNode::default());
        let mut tree = ProjectNode::default();
        tree.children.insert("src".to_owned(), source);
        let rules = CompiledModelTreeRules::new(
            &[ModelTreeRule {
                pattern: "**".to_owned(),
                format: None,
            }],
            &tree,
        )
        .unwrap();

        assert_eq!(rules.middleware_for("src"), None);
        assert_eq!(rules.middleware_for("src/Shared"), None);
        assert_eq!(rules.middleware_for("src/Other"), Some(Middleware::Rbxm));
        assert_eq!(
            rules.middleware_for("src/Shared/Child"),
            Some(Middleware::Rbxm)
        );
    }

    fn on_disk(
        name: &str,
        middleware: Middleware,
        path: &str,
        children: Vec<InstanceSnapshot>,
    ) -> InstanceSnapshot {
        InstanceSnapshot::new()
            .name(name)
            .class_name("Folder")
            .children(children)
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(Path::new(path))
                    .middleware(middleware),
            )
    }

    /// A script-free `Holder` directory with an `FX` model file and a `Side`
    /// directory. Returns the tree with the IDs of its root and of `Holder`.
    fn holder_tree() -> (RojoTree, Ref, Ref) {
        let tree = RojoTree::new(on_disk(
            "root",
            Middleware::Dir,
            "/root",
            vec![on_disk(
                "Holder",
                Middleware::Dir,
                "/root/Holder",
                vec![
                    on_disk("FX", Middleware::Rbxm, "/root/Holder/FX.rbxm", Vec::new()),
                    on_disk("Side", Middleware::Dir, "/root/Holder/Side", Vec::new()),
                ],
            )],
        ));
        let root = tree.get_root_id();
        let holder = tree.get_instance(root).unwrap().children()[0];
        (tree, root, holder)
    }

    #[test]
    fn layouts_that_follow_the_rules_are_left_alone() {
        let (tree, _, _) = holder_tree();
        let rules = rules(&[("**/FX", None)]);
        let trees = ModelTrees::find(&rules, tree.inner());

        for as_directories in [false, true] {
            assert!(needs_relayout(&rules, &tree, as_directories).is_empty());
        }
        assert!(inline::needs_relayout(&tree, &trees).is_empty());
    }

    #[test]
    fn script_free_directories_inline_without_a_model_tree() {
        let (tree, root, holder) = holder_tree();
        assert_eq!(
            inline::needs_relayout(&tree, &ModelTrees::default()),
            HashSet::from([root, holder])
        );
    }

    #[test]
    fn stale_layouts_are_reconciled_with_their_ancestors() {
        let (tree, root, holder) = holder_tree();
        let stale = HashSet::from([root, holder]);

        // The model file is in another format than its rule asks for.
        let other_format = rules(&[("**/FX", Some(ModelTreeFormat::Rbxmx))]);
        // A directory matches a rule.
        let directory = rules(&[("**/FX", None), ("Holder/Side", None)]);
        for as_directories in [false, true] {
            assert_eq!(needs_relayout(&other_format, &tree, as_directories), stale);
            assert_eq!(needs_relayout(&directory, &tree, as_directories), stale);
        }

        // No rule matches the model file. Only `instancesAsDirectories` asks
        // for it to become a directory again.
        let none = rules(&[]);
        assert_eq!(needs_relayout(&none, &tree, true), stale);
        assert!(needs_relayout(&none, &tree, false).is_empty());
    }

    #[test]
    fn single_star_stays_within_one_instance_name() {
        let rules = rules(&[("Cutscenes/*/FX", None)]);
        assert_eq!(
            rules.middleware_for("Cutscenes/Intro/FX"),
            Some(Middleware::Rbxm)
        );
        assert_eq!(rules.middleware_for("Cutscenes/Intro/Scene/FX"), None);
        assert_eq!(rules.middleware_for("Cutscenes/Intro/FX/Spark"), None);
        assert_eq!(rules.middleware_for("Cutscenes/Intro"), None);
    }

    #[test]
    fn double_star_crosses_instance_names() {
        let rules = rules(&[("**/FX", None)]);
        assert_eq!(rules.middleware_for("FX"), Some(Middleware::Rbxm));
        assert_eq!(rules.middleware_for("A/B/C/FX"), Some(Middleware::Rbxm));
        assert_eq!(rules.middleware_for("A/FXS"), None);
    }

    #[test]
    fn first_matching_rule_picks_the_format() {
        let rules = rules(&[
            ("Maps/Text/*", Some(ModelTreeFormat::Rbxmx)),
            ("Maps/**", Some(ModelTreeFormat::Rbxm)),
        ]);
        assert_eq!(
            rules.middleware_for("Maps/Text/One"),
            Some(Middleware::Rbxmx)
        );
        assert_eq!(rules.middleware_for("Maps/Other"), Some(Middleware::Rbxm));
        assert_eq!(rules.middleware_for("Elsewhere"), None);
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        let invalid = [ModelTreeRule {
            pattern: "Maps/[".to_owned(),
            format: None,
        }];
        assert!(CompiledModelTreeRules::new(&invalid, &ProjectNode::default()).is_err());
    }

    #[test]
    fn rules_deserialize_with_an_optional_format() {
        let parsed: Vec<ModelTreeRule> = serde_json::from_str(
            r#"[{ "pattern": "A/*" }, { "pattern": "B", "format": "rbxmx" }]"#,
        )
        .unwrap();
        assert_eq!(parsed[0].format, None);
        assert_eq!(parsed[1].format, Some(ModelTreeFormat::Rbxmx));
        assert!(
            serde_json::from_str::<ModelTreeRule>(r#"{ "pattern": "A", "format": "rbxl" }"#)
                .is_err()
        );
        assert!(serde_json::from_str::<ModelTreeRule>(r#"{ "glob": "A" }"#).is_err());
    }

    #[test]
    fn matches_are_found_at_every_depth() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let scenes = dom.insert(root, InstanceBuilder::new("Folder").with_name("Scenes"));
        let intro = dom.insert(scenes, InstanceBuilder::new("Folder").with_name("Intro"));
        let effects = dom.insert(intro, InstanceBuilder::new("Folder").with_name("FX"));
        let nested = dom.insert(effects, InstanceBuilder::new("Folder").with_name("FX"));
        let outro = dom.insert(scenes, InstanceBuilder::new("Folder").with_name("Outro"));
        let other = dom.insert(root, InstanceBuilder::new("Folder").with_name("Other"));

        let trees = ModelTrees::find(&rules(&[("**/FX", None)]), &dom);

        for referent in [effects, nested] {
            assert_eq!(trees.middleware(referent), Some(Middleware::Rbxm));
        }
        for referent in [root, scenes, intro, outro, other] {
            assert_eq!(trees.middleware(referent), None);
        }
        for referent in [root, scenes, intro, effects, nested] {
            assert!(trees.contains(referent));
        }
        for referent in [outro, other] {
            assert!(!trees.contains(referent));
        }
    }

    #[test]
    fn no_rules_match_nothing() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let child = dom.insert(dom.root_ref(), InstanceBuilder::new("Folder"));
        let trees = ModelTrees::find(&rules(&[]), &dom);
        assert_eq!(trees.middleware(child), None);
        assert!(!trees.contains(dom.root_ref()));
    }
}
