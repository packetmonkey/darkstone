use clap::Parser as ClapParser;
use itertools::Itertools;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tree_sitter::{Parser, Query, QueryCursor, Tree};
use tree_sitter_md::{MarkdownParser, MarkdownTree};

#[derive(Debug)]
struct QueryCache {
    body_query: Query,
    alias_query: Query,
    frontmatter_links: Query,
}

#[derive(Debug)]
struct Vault {
    notes: Vec<Note>,
}

impl Vault {
    fn new(path: PathBuf) -> Self {
        let query_cache = Arc::new(QueryCache {
            body_query: Query::new(
                &tree_sitter_md::inline_language(),
                "(
                    (
                        _
                        [
                            ((link_destination)? @destination (link_text) @text)
                            ((link_destination) @destination (link_text)? @text)
                        ]
                    )
                )",
            )
            .unwrap(),
            alias_query: Query::new(
                &tree_sitter_yaml::language(),
                "
                            (
                              (block_mapping_pair
                                key: ((flow_node) @key (#match? @key \"aliases\"))
                                value: (block_node
                                  (block_sequence
                                    (block_sequence_item
                                      (flow_node
                                        (plain_scalar
                                          (string_scalar)+ @aliases)))))
                              )
                            )
                        ",
            )
            .unwrap(),
            frontmatter_links: Query::new(
                &tree_sitter_yaml::language(),
                "((double_quote_scalar) @scalar)",
            )
            .unwrap(),
        });

        let glob = format!("{}/**/*.md", path.as_os_str().to_str().unwrap());
        let notes = glob::glob(&glob)
            .unwrap()
            .into_iter()
            .par_bridge()
            .collect::<Result<Vec<PathBuf>, glob::GlobError>>()
            .unwrap()
            .into_iter()
            .par_bridge()
            .map(|p| Note::new(p, query_cache.clone()))
            .collect();

        Self { notes }
    }

    fn notes(&self) -> Vec<Note> {
        self.notes.clone()
    }

    fn links(&self) -> Vec<Link> {
        self.notes().iter().map(|n| n.links()).flatten().collect()
    }

    fn targets(&self) -> Vec<String> {
        self.notes()
            .par_iter()
            .map(|n| n.targets())
            .flatten()
            .collect::<Vec<String>>()
            .into_iter()
            .unique()
            .collect()
    }
}

#[derive(Clone, Debug)]
struct Note {
    path: PathBuf,
    content: String,
    tree: MarkdownTree,
    query_cache: Arc<QueryCache>,
}

impl Note {
    fn new(path: PathBuf, query_cache: Arc<QueryCache>) -> Self {
        let content = std::fs::read_to_string(&path).unwrap();
        let mut parser = MarkdownParser::default();
        let tree = parser.parse(content.as_bytes(), None).unwrap();

        Self {
            path,
            content,
            tree,
            query_cache,
        }
    }

    fn targets(&self) -> Vec<String> {
        let mut targets = vec![self.name()];
        targets.append(&mut self.alias_targets());

        let mut destinations = self.links().iter().map(|l| l.destination.clone()).collect();
        targets.append(&mut destinations);

        targets
    }

    fn name(&self) -> String {
        self.path
            .file_stem()
            .unwrap()
            .to_owned()
            .to_str()
            .unwrap()
            .to_string()
    }

    fn alias_targets(&self) -> Vec<String> {
        let name = self.name();

        self.aliases()
            .par_iter()
            .map(|alias| format!("{}|{}", name, alias))
            .collect()
    }

    fn aliases(&self) -> Vec<String> {
        match self.parsed_frontmatter() {
            Some(frontmatter) => {
                let mut aliases = vec![];
                // dbg!(frontmatter.root_node().to_sexp());
                // dbg!(&alias_query);
                let mut query_cursor = QueryCursor::new();
                let frontmatter_content = self.frontmatter().unwrap().clone();
                let matches = query_cursor.matches(
                    &self.query_cache.alias_query,
                    frontmatter.root_node().clone(),
                    frontmatter_content.as_bytes(),
                );

                for found_match in matches {
                    // dbg!(&found_match);
                    let nodes = found_match
                        .nodes_for_capture_index(1)
                        .into_iter()
                        .par_bridge()
                        .collect::<Vec<tree_sitter::Node>>();

                    // dbg!(nodes);
                    for node in nodes {
                        let alias = frontmatter_content[node.byte_range()].to_string();
                        // dbg!(&alias);
                        aliases.push(alias);
                    }
                }

                aliases
            }
            None => vec![],
        }
    }

    fn parsed_frontmatter(&self) -> Option<Tree> {
        match self.frontmatter() {
            Some(frontmatter) => {
                let mut parser = Parser::new();
                parser
                    .set_language(&tree_sitter_yaml::language())
                    .expect("Error loading Markdown grammar");

                let tree: tree_sitter::Tree = parser.parse(frontmatter, None).unwrap();

                Some(tree)
            }
            None => None,
        }
    }

    fn frontmatter(&self) -> Option<String> {
        let mut cursor = self.tree.walk();
        cursor.goto_first_child();
        let node = cursor.node();

        match node.kind() {
            "minus_metadata" => Some(self.content[node.start_byte()..node.end_byte()].to_string()),
            _ => None,
        }
    }

    fn links(&self) -> Vec<Link> {
        let mut links = self.frontmatter_links();
        links.append(&mut self.body_links(&self.tree, self.content.clone()));

        links
    }

    fn frontmatter_links(&self) -> Vec<Link> {
        let mut links = std::vec![];
        let mut query_cursor = QueryCursor::new();

        match self.parsed_frontmatter() {
            Some(tree) => {
                let frontmatter = self.frontmatter().unwrap();
                let matches = query_cursor.matches(
                    &self.query_cache.frontmatter_links,
                    tree.root_node(),
                    frontmatter.as_bytes(),
                );

                for found_match in matches {
                    let node = found_match
                        .nodes_for_capture_index(0)
                        .collect::<Vec<tree_sitter::Node>>()
                        .pop()
                        .unwrap();

                    let scalar = self.content[node.byte_range()].to_string();
                    let mut parser = MarkdownParser::default();
                    let tree = parser.parse(scalar.as_bytes(), None).unwrap();
                    let mut parsed_links = self.body_links(&tree, scalar.clone());
                    links.append(&mut parsed_links);
                }

                links
            }
            None => std::vec![],
        }
    }

    fn body_links(&self, tree: &MarkdownTree, content: String) -> Vec<Link> {
        let mut links = std::vec![];

        for inline_tree in tree.inline_trees() {
            let mut query_cursor = QueryCursor::new();
            let matches = query_cursor.matches(
                &self.query_cache.body_query,
                inline_tree.root_node(),
                content.as_bytes(),
            );

            for found_match in matches {
                // index 0 is destination
                // index 1 is text
                //
                // Frontmatter "[[Foo]]" will come in as a wiki_link with just a destination
                // Frontmatter "[[Foo|bar]]" will come in as a wiki_link with text and destination
                // Body [[Bar]] will come in as a shortcut_link with just text
                // then body [[dest|name]] will come in as wiki_link with text and destination
                let dest_node = found_match
                    .nodes_for_capture_index(0)
                    .collect::<Vec<tree_sitter::Node>>()
                    .pop();

                let text_node = found_match
                    .nodes_for_capture_index(1)
                    .collect::<Vec<tree_sitter::Node>>()
                    .pop();

                // If there is no destination, text is used for both
                // If there is no text, destination is used for both
                let dest_node = dest_node.unwrap_or_else(|| text_node.unwrap());
                let text_node = text_node.unwrap_or_else(|| dest_node);

                let destination = content[dest_node.byte_range()].to_string();
                let text = content[text_node.byte_range()].to_string();

                links.push(Link { destination, text });
            }
        }

        links
    }
}

#[derive(Debug)]
struct Link {
    destination: String,
    text: String,
}

/// Simple program to greet a person
#[derive(ClapParser, Debug)]
#[command()]
struct Args {
    #[arg(short, long)]
    vault_path: PathBuf,
}

fn main() {
    let args = Args::parse();
    let vault = Vault::new(args.vault_path);

    // let notes = vault.notes();
    // let note = notes
    //     .iter()
    //     .find(|n| n.name() == "1716397370-SYPL")
    //     .unwrap();
    // dbg!(note.targets());

    let mut targets = vault.targets();
    targets.sort();
    for target in targets {
        println!("{}", target);
    }

    // for link in vault.links() {
    //     println!("{} -> {}", link.text, link.destination);
    // }

    // println!("Notes: {}", vault.notes().len());
    // println!("Links: {}", vault.links().len());
}
