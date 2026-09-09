use std::env;
use std::path::PathBuf;
use tine_core::model::Graph;
use tine_core::publish::publish_graph;

fn main() {
    let root = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: publish_security_fixture <graph-root>");
    let projection_dir = tempfile::tempdir().expect("create derived-state directory");
    let graph = Graph::open(&root);
    graph
        .attach_direct_projection(projection_dir.path().join("direct.sqlite"))
        .expect("open main projection");
    graph.warm_cache();
    let (output, count) = publish_graph(&graph).expect("publish security fixture");
    println!("{count}\n{output}");
}
