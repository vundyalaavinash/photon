use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Code,
    Config,
    Docs,
    Test,
    Unanalysable,
}

pub fn classify_path(path: &str) -> FileKind {
    let name = file_name(path);
    let directories = path
        .rsplit_once('/')
        .map_or("", |(directories, _)| directories);
    let in_test_directory = directories
        .split('/')
        .any(|segment| matches!(segment, "test" | "tests" | "__tests__" | "spec"));
    let stem_ends_with_test = name
        .rsplit_once('.')
        .is_some_and(|(stem, _)| stem.ends_with("_test"));
    if in_test_directory
        || stem_ends_with_test
        || name.contains(".test.")
        || name.contains(".spec.")
        || (name.starts_with("test_") && name.ends_with(".py"))
    {
        return FileKind::Test;
    }

    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(
        extension.as_str(),
        "json" | "yaml" | "yml" | "toml" | "ini" | "env" | "lock" | "xml"
    ) || matches!(name, "Dockerfile" | "Makefile" | ".gitignore")
        || path.starts_with(".github/")
        || path.contains("/.github/")
    {
        return FileKind::Config;
    }
    if matches!(extension.as_str(), "md" | "mdx" | "rst" | "txt") {
        return FileKind::Docs;
    }
    FileKind::Code
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[derive(Clone, Debug)]
pub struct MapFile {
    pub path: String,
    pub kind: FileKind,
    pub status_label: &'static str,
}

#[derive(Clone, Debug)]
pub struct MapFunction {
    pub path: String,
    pub name: String,
    pub first_row: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub files: Vec<String>,
    pub functions: Vec<usize>,
}

pub fn build_review_map(
    files: &[MapFile],
    functions: &[MapFunction],
    edges: &[(usize, usize)],
) -> Vec<Step> {
    let status_labels: BTreeMap<&str, &'static str> = files
        .iter()
        .map(|file| (file.path.as_str(), file.status_label))
        .collect();
    let status_of = |path: &str| status_labels.get(path).copied().unwrap_or("changed");
    let kinds: BTreeMap<&str, FileKind> = files
        .iter()
        .map(|file| (file.path.as_str(), file.kind))
        .collect();
    let kind_of = |path: &str| kinds.get(path).copied().unwrap_or(FileKind::Code);

    let is_node: Vec<bool> = functions
        .iter()
        .map(|function| kind_of(&function.path) == FileKind::Code)
        .collect();
    let node_at = |index: usize| is_node.get(index).copied().unwrap_or(false);

    // Self-edges (recursion) carry no ordering information and would otherwise
    // look like an incoming edge or a one-node cycle.
    let edges: BTreeSet<(usize, usize)> = edges
        .iter()
        .copied()
        .filter(|&(caller, callee)| caller != callee && node_at(caller) && node_at(callee))
        .collect();

    let sort_key = |index: usize| {
        functions
            .get(index)
            .map(|function| {
                (
                    function.path.as_str(),
                    function.first_row,
                    function.name.as_str(),
                )
            })
            .unwrap_or_default()
    };

    let mut union_find = UnionFind::new(functions.len());
    for &(caller, callee) in &edges {
        if sort_key(caller).0 == sort_key(callee).0 {
            union_find.union(caller, callee);
        }
    }
    let connected: BTreeSet<usize> = edges
        .iter()
        .flat_map(|&(caller, callee)| [caller, callee])
        .collect();
    let mut first_isolated_in_file: BTreeMap<&str, usize> = BTreeMap::new();
    for index in (0..functions.len()).filter(|&index| node_at(index) && !connected.contains(&index))
    {
        let first = *first_isolated_in_file
            .entry(sort_key(index).0)
            .or_insert(index);
        union_find.union(first, index);
    }

    // Merging by file can make the cluster graph cyclic even when the function
    // graph is not, so keep merging cyclic clusters until it is a DAG.
    let (clusters, cluster_of, cluster_edges) = loop {
        let mut members_by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for index in (0..functions.len()).filter(|&index| node_at(index)) {
            members_by_root
                .entry(union_find.find(index))
                .or_default()
                .push(index);
        }
        let mut clusters: Vec<Vec<usize>> = members_by_root.into_values().collect();
        for members in &mut clusters {
            members.sort_by_key(|&index| sort_key(index));
        }
        clusters.sort_by_key(|members| members.first().map(|&index| sort_key(index)));

        let mut cluster_of = vec![usize::MAX; functions.len()];
        for (cluster_index, members) in clusters.iter().enumerate() {
            for &member in members {
                if let Some(slot) = cluster_of.get_mut(member) {
                    *slot = cluster_index;
                }
            }
        }
        let cluster_edges: BTreeSet<(usize, usize)> = edges
            .iter()
            .map(|&(caller, callee)| (cluster_of[caller], cluster_of[callee]))
            .filter(|(caller, callee)| caller != callee)
            .collect();
        let mut callees_of = vec![Vec::new(); clusters.len()];
        for &(caller, callee) in &cluster_edges {
            callees_of[caller].push(callee);
        }
        let cycles = strongly_connected_components(&callees_of);
        if cycles.is_empty() {
            break (clusters, cluster_of, cluster_edges);
        }
        for cycle in cycles {
            let mut first_functions = cycle
                .iter()
                .filter_map(|&cluster| clusters[cluster].first());
            if let Some(&anchor) = first_functions.next() {
                for &other in first_functions {
                    union_find.union(anchor, other);
                }
            }
        }
    };

    let mut pending_callees = vec![0usize; clusters.len()];
    let mut callers_of = vec![Vec::new(); clusters.len()];
    for &(caller, callee) in &cluster_edges {
        pending_callees[caller] += 1;
        callers_of[callee].push(caller);
    }

    // Cluster indices are already in (path, row) order, so the smallest ready
    // index is the tie-break winner.
    let mut ready: BTreeSet<usize> = (0..clusters.len())
        .filter(|&cluster| pending_callees[cluster] == 0)
        .collect();
    let mut order = Vec::with_capacity(clusters.len());
    while let Some(cluster) = ready.pop_first() {
        order.push(cluster);
        for &caller in &callers_of[cluster] {
            pending_callees[caller] -= 1;
            if pending_callees[caller] == 0 {
                ready.insert(caller);
            }
        }
    }
    // Unreachable while the loop above guarantees a DAG; never drop a function.
    if order.len() < clusters.len() {
        let emitted: BTreeSet<usize> = order.iter().copied().collect();
        order.extend((0..clusters.len()).filter(|cluster| !emitted.contains(cluster)));
    }

    let mut has_in_cluster_caller = vec![false; functions.len()];
    for &(caller, callee) in &edges {
        if cluster_of[caller] == cluster_of[callee] {
            has_in_cluster_caller[callee] = true;
        }
    }

    let mut steps = Vec::new();
    for cluster in order {
        let members = &clusters[cluster];
        let main = members
            .iter()
            .copied()
            .find(|&member| !has_in_cluster_caller[member])
            .or_else(|| members.first().copied());
        let Some(main) = main.and_then(|main| functions.get(main)) else {
            continue;
        };
        let mut qualified_names: Vec<String> = members
            .iter()
            .map(|&member| format!("{}::{}", sort_key(member).0, sort_key(member).2))
            .collect();
        qualified_names.sort();
        let mut file_paths: Vec<String> = members
            .iter()
            .map(|&member| sort_key(member).0.to_string())
            .collect();
        file_paths.sort();
        file_paths.dedup();
        steps.push(Step {
            key: qualified_names.join(","),
            title: format!("{} · {}()", file_name(&main.path), main.name),
            subtitle: format!("{} · {}", main.path, status_of(&main.path)),
            files: file_paths,
            functions: members.clone(),
        });
    }

    let has_functions: BTreeSet<&str> = functions
        .iter()
        .enumerate()
        .filter(|&(index, _)| node_at(index))
        .map(|(_, function)| function.path.as_str())
        .collect();
    let mut file_steps: BTreeMap<&str, &MapFile> = BTreeMap::new();
    for file in files {
        let wants_step = match file.kind {
            FileKind::Code => !has_functions.contains(file.path.as_str()),
            FileKind::Unanalysable | FileKind::Config | FileKind::Docs => true,
            FileKind::Test => false,
        };
        if wants_step {
            file_steps.insert(file.path.as_str(), file);
        }
    }
    for (path, file) in file_steps {
        steps.push(Step {
            key: path.to_string(),
            title: file_name(path).to_string(),
            subtitle: format!("{path} · {}", file.status_label),
            files: vec![path.to_string()],
            functions: Vec::new(),
        });
    }

    let test_paths: BTreeSet<&str> = files
        .iter()
        .filter(|file| file.kind == FileKind::Test)
        .map(|file| file.path.as_str())
        .collect();
    if !test_paths.is_empty() {
        let mut test_functions: Vec<usize> = (0..functions.len())
            .filter(|&index| test_paths.contains(sort_key(index).0))
            .collect();
        test_functions.sort_by_key(|&index| sort_key(index));
        let count = test_paths.len();
        steps.push(Step {
            key: "tests".to_string(),
            title: "Tests".to_string(),
            subtitle: format!("{count} test file{}", if count == 1 { "" } else { "s" }),
            files: test_paths.into_iter().map(str::to_string).collect(),
            functions: test_functions,
        });
    }
    steps
}

struct UnionFind {
    parents: Vec<usize>,
}

impl UnionFind {
    fn new(size: usize) -> Self {
        Self {
            parents: (0..size).collect(),
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let mut root = index;
        while let Some(&parent) = self.parents.get(root) {
            if parent == root {
                break;
            }
            root = parent;
        }
        let mut current = index;
        while current != root {
            let Some(parent) = self.parents.get_mut(current) else {
                break;
            };
            current = std::mem::replace(parent, root);
        }
        root
    }

    fn union(&mut self, first: usize, second: usize) {
        let (first_root, second_root) = (self.find(first), self.find(second));
        if let Some(parent) = self.parents.get_mut(second_root) {
            *parent = first_root;
        }
    }
}

/// Iterative Tarjan (an explicit stack, so deep call chains cannot overflow
/// the thread stack); components with a single member are omitted.
fn strongly_connected_components(callees_of: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let size = callees_of.len();
    let mut indices: Vec<Option<usize>> = vec![None; size];
    let mut low_links = vec![0usize; size];
    let mut on_stack = vec![false; size];
    let mut component_stack = Vec::new();
    let mut components = Vec::new();
    let mut next_index = 0;

    for root in 0..size {
        if indices[root].is_some() {
            continue;
        }
        let mut call_stack = vec![(root, 0usize)];
        indices[root] = Some(next_index);
        low_links[root] = next_index;
        next_index += 1;
        component_stack.push(root);
        on_stack[root] = true;

        while let Some(&mut (node, ref mut next_edge)) = call_stack.last_mut() {
            if let Some(&callee) = callees_of[node].get(*next_edge) {
                *next_edge += 1;
                match indices[callee] {
                    None => {
                        indices[callee] = Some(next_index);
                        low_links[callee] = next_index;
                        next_index += 1;
                        component_stack.push(callee);
                        on_stack[callee] = true;
                        call_stack.push((callee, 0));
                    }
                    Some(callee_index) if on_stack[callee] => {
                        low_links[node] = low_links[node].min(callee_index);
                    }
                    Some(_) => {}
                }
                continue;
            }

            call_stack.pop();
            if let Some(&(parent, _)) = call_stack.last() {
                low_links[parent] = low_links[parent].min(low_links[node]);
            }
            if indices[node] == Some(low_links[node]) {
                let mut component = Vec::new();
                while let Some(member) = component_stack.pop() {
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                if component.len() > 1 {
                    components.push(component);
                }
            }
        }
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, kind: FileKind) -> MapFile {
        MapFile {
            path: path.to_string(),
            kind,
            status_label: "changed",
        }
    }

    fn function(path: &str, name: &str, first_row: u32) -> MapFunction {
        MapFunction {
            path: path.to_string(),
            name: name.to_string(),
            first_row,
        }
    }

    fn keys(steps: &[Step]) -> Vec<&str> {
        steps.iter().map(|step| step.key.as_str()).collect()
    }

    #[test]
    fn classify_paths() {
        let cases = [
            ("src/tests/a.rs", FileKind::Test),
            ("test/a.rs", FileKind::Test),
            ("web/__tests__/a.ts", FileKind::Test),
            ("spec/a.rb", FileKind::Test),
            ("src/foo_test.go", FileKind::Test),
            ("src/foo.test.ts", FileKind::Test),
            ("src/foo.spec.ts", FileKind::Test),
            ("pkg/test_foo.py", FileKind::Test),
            ("test_foo.rs", FileKind::Code),
            ("package.json", FileKind::Config),
            ("a/b.yaml", FileKind::Config),
            ("a/b.yml", FileKind::Config),
            ("Cargo.toml", FileKind::Config),
            ("a.ini", FileKind::Config),
            (".env", FileKind::Config),
            ("Cargo.lock", FileKind::Config),
            ("pom.xml", FileKind::Config),
            ("docker/Dockerfile", FileKind::Config),
            ("Makefile", FileKind::Config),
            (".gitignore", FileKind::Config),
            (".github/workflows/ci.rs", FileKind::Config),
            ("README.md", FileKind::Docs),
            ("a.mdx", FileKind::Docs),
            ("a.rst", FileKind::Docs),
            ("a.txt", FileKind::Docs),
            ("src/main.rs", FileKind::Code),
            ("contest/main.rs", FileKind::Code),
        ];
        for (path, expected) in cases {
            assert_eq!(classify_path(path), expected, "{path}");
        }
    }

    #[test]
    fn empty_pr_has_no_steps() {
        assert!(build_review_map(&[], &[], &[]).is_empty());
    }

    #[test]
    fn callees_before_callers_across_files() {
        let files = [
            file("login.ts", FileKind::Code),
            file("rate_limit.ts", FileKind::Code),
        ];
        let functions = [
            function("login.ts", "handleLogin", 3),
            function("rate_limit.ts", "allow", 1),
        ];
        let steps = build_review_map(&files, &functions, &[(0, 1)]);
        assert_eq!(
            keys(&steps),
            ["rate_limit.ts::allow", "login.ts::handleLogin"]
        );
        assert_eq!(steps[0].title, "rate_limit.ts · allow()");
        assert_eq!(steps[0].subtitle, "rate_limit.ts · changed");
        assert_eq!(steps[0].files, ["rate_limit.ts"]);
        assert_eq!(steps[0].functions, [1]);
    }

    #[test]
    fn same_file_connected_functions_cluster() {
        let files = [file("a/x.rs", FileKind::Code)];
        let functions = [
            function("a/x.rs", "helper", 1),
            function("a/x.rs", "entry", 10),
        ];
        let steps = build_review_map(&files, &functions, &[(1, 0)]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].key, "a/x.rs::entry,a/x.rs::helper");
        assert_eq!(steps[0].title, "x.rs · entry()");
        assert_eq!(steps[0].subtitle, "a/x.rs · changed");
        assert_eq!(steps[0].functions, [0, 1]);
    }

    #[test]
    fn isolated_functions_in_one_file_share_a_step() {
        let files = [file("x.rs", FileKind::Code)];
        let functions = [function("x.rs", "b", 20), function("x.rs", "a", 5)];
        let steps = build_review_map(&files, &functions, &[]);
        assert_eq!(keys(&steps), ["x.rs::a,x.rs::b"]);
        assert_eq!(steps[0].title, "x.rs · a()");
    }

    #[test]
    fn cross_file_cycle_merges_into_one_step() {
        let files = [file("a.rs", FileKind::Code), file("b.rs", FileKind::Code)];
        let functions = [function("a.rs", "ping", 1), function("b.rs", "pong", 1)];
        let steps = build_review_map(&files, &functions, &[(0, 1), (1, 0)]);
        assert_eq!(keys(&steps), ["a.rs::ping,b.rs::pong"]);
        assert_eq!(steps[0].files, ["a.rs", "b.rs"]);
    }

    #[test]
    fn self_edges_are_ignored() {
        let files = [file("a.rs", FileKind::Code), file("b.rs", FileKind::Code)];
        let functions = [function("a.rs", "f", 1), function("b.rs", "g", 1)];
        let steps = build_review_map(&files, &functions, &[(0, 0), (0, 1), (1, 1)]);
        assert_eq!(keys(&steps), ["b.rs::g", "a.rs::f"]);
    }

    #[test]
    fn diamond_orders_callees_first_with_tie_break() {
        let files = [
            file("a.rs", FileKind::Code),
            file("b.rs", FileKind::Code),
            file("c.rs", FileKind::Code),
            file("d.rs", FileKind::Code),
        ];
        let functions = [
            function("a.rs", "a", 1),
            function("b.rs", "b", 1),
            function("c.rs", "c", 1),
            function("d.rs", "d", 1),
        ];
        let steps = build_review_map(&files, &functions, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        assert_eq!(keys(&steps), ["d.rs::d", "b.rs::b", "c.rs::c", "a.rs::a"]);
    }

    #[test]
    fn non_code_after_clusters_tests_last() {
        let files = [
            file("t/one.test.ts", FileKind::Test),
            file("z.md", FileKind::Docs),
            file("a.rs", FileKind::Code),
            file("m.json", FileKind::Config),
            file("img.png", FileKind::Unanalysable),
            file("tests/two.rs", FileKind::Test),
            file("tests/three.rs", FileKind::Test),
        ];
        let functions = [
            function("a.rs", "run", 1),
            function("tests/two.rs", "checks_run", 1),
        ];
        let steps = build_review_map(&files, &functions, &[(1, 0)]);
        assert_eq!(
            keys(&steps),
            ["a.rs::run", "img.png", "m.json", "z.md", "tests"]
        );
        let tests = &steps[4];
        assert_eq!(tests.title, "Tests");
        assert_eq!(tests.subtitle, "3 test files");
        assert_eq!(tests.functions, [1]);
        assert_eq!(steps[1].title, "img.png");
        assert_eq!(steps[1].subtitle, "img.png · changed");
    }

    #[test]
    fn only_test_files() {
        let files = [file("tests/a.rs", FileKind::Test)];
        let steps = build_review_map(&files, &[], &[]);
        assert_eq!(keys(&steps), ["tests"]);
        assert_eq!(steps[0].subtitle, "1 test file");
    }

    #[test]
    fn only_config_files() {
        let files = [
            file("b.toml", FileKind::Config),
            file("a.json", FileKind::Config),
        ];
        assert_eq!(
            keys(&build_review_map(&files, &[], &[])),
            ["a.json", "b.toml"]
        );
    }

    #[test]
    fn file_level_only_code_file_is_its_own_step() {
        let files = [file("a.rs", FileKind::Code), file("b.rs", FileKind::Code)];
        let functions = [function("b.rs", "f", 1)];
        let steps = build_review_map(&files, &functions, &[]);
        assert_eq!(keys(&steps), ["b.rs::f", "a.rs"]);
        assert_eq!(steps[1].title, "a.rs");
    }

    #[test]
    fn ordering_is_deterministic() {
        let files = [
            file("a.rs", FileKind::Code),
            file("b.rs", FileKind::Code),
            file("c.rs", FileKind::Code),
            file("d.md", FileKind::Docs),
            file("e.rs", FileKind::Code),
        ];
        let functions = [
            function("a.rs", "a", 1),
            function("b.rs", "b", 1),
            function("c.rs", "c1", 1),
            function("c.rs", "c2", 9),
            function("b.rs", "b2", 7),
        ];
        let edges = [(0, 1), (0, 2), (1, 3), (4, 3)];
        let baseline = build_review_map(&files, &functions, &edges);

        // Reversing the inputs relabels every function index.
        let reversed_files: Vec<_> = files.iter().rev().cloned().collect();
        let reversed_functions: Vec<_> = functions.iter().rev().cloned().collect();
        let last = functions.len() - 1;
        let reversed_edges: Vec<_> = edges
            .iter()
            .rev()
            .map(|&(caller, callee)| (last - caller, last - callee))
            .collect();
        let shuffled = build_review_map(&reversed_files, &reversed_functions, &reversed_edges);
        assert_eq!(keys(&baseline), keys(&shuffled));
    }

    #[test]
    fn weakly_connected_file_clusters_forming_a_cycle_are_merged() {
        let files = [file("a.rs", FileKind::Code), file("b.rs", FileKind::Code)];
        let functions = [
            function("a.rs", "f", 1),
            function("a.rs", "g", 2),
            function("a.rs", "z", 3),
            function("b.rs", "p", 1),
            function("b.rs", "q", 2),
            function("b.rs", "r", 3),
        ];
        let edges = [(0, 2), (1, 2), (3, 5), (4, 5), (0, 3), (4, 1)];
        let steps = build_review_map(&files, &functions, &edges);
        assert_eq!(steps.len(), 1);
        assert_every_function_once(&steps, functions.len());
    }

    fn assert_every_function_once(steps: &[Step], function_count: usize) {
        let mut seen: Vec<usize> = steps
            .iter()
            .flat_map(|step| step.functions.iter().copied())
            .collect();
        seen.sort();
        assert_eq!(seen, (0..function_count).collect::<Vec<_>>());
    }

    #[test]
    fn random_graphs_keep_every_function_exactly_once() {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        for _ in 0..200 {
            let file_count = 1 + next(4);
            let files: Vec<MapFile> = (0..file_count)
                .map(|index| file(&format!("f{index}.rs"), FileKind::Code))
                .collect();
            let function_count = 1 + next(12);
            let functions: Vec<MapFunction> = (0..function_count)
                .map(|index| {
                    function(
                        &format!("f{}.rs", next(file_count)),
                        &format!("n{index}"),
                        index as u32,
                    )
                })
                .collect();
            let edges: Vec<(usize, usize)> = (0..next(25))
                .map(|_| (next(function_count), next(function_count)))
                .collect();
            let steps = build_review_map(&files, &functions, &edges);
            assert_every_function_once(&steps, function_count);
        }
    }

    #[test]
    fn long_call_chain_does_not_overflow() {
        let count = 200_000;
        let files = [file("a.rs", FileKind::Code)];
        let functions: Vec<MapFunction> = (0..count)
            .map(|index| function("a.rs", &format!("n{index}"), index as u32))
            .collect();
        let edges: Vec<(usize, usize)> = (0..count - 1).map(|index| (index, index + 1)).collect();
        assert_eq!(build_review_map(&files, &functions, &edges).len(), 1);
    }
}
