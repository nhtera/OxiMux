//! Local-only probe: does the block tree account for every word of the source?
//!
//! The tight-list bug parsed a structurally correct list whose items were empty
//! — the shape was right and the content was gone, which no shape assertion
//! catches. This walks a corpus and reports words present in the source that
//! appear nowhere in the tree. Not a committed test: it is pointed at the
//! developer's own agent sessions.
use std::collections::HashSet;

use oximux_markdown::{Block, InlineRun, TopBlock, parse_full};

fn runs_text(runs: &[InlineRun]) -> String {
    // Link destinations count as accounted for: they are carried on the run's
    // style, not in its text, and the reader sees the label rather than the URL.
    runs.iter()
        .map(|r| match &r.style.link {
            Some(url) => format!("{} {url} ", r.text),
            None => r.text.clone(),
        })
        .collect()
}

fn collect(block: &Block, out: &mut String) {
    match block {
        Block::Paragraph { runs } | Block::Heading { runs, .. } => {
            out.push_str(&runs_text(runs));
            out.push(' ');
        }
        Block::CodeBlock { code, language } => {
            if let Some(l) = language {
                out.push_str(l);
                out.push(' ');
            }
            out.push_str(code);
            out.push(' ');
        }
        Block::BlockQuote { children } => children.iter().for_each(|c| collect(c, out)),
        Block::List { items, .. } => items.iter().flatten().for_each(|b| collect(b, out)),
        Block::Table { header, rows, .. } => {
            for cell in header.iter().chain(rows.iter().flatten()) {
                out.push_str(&runs_text(cell));
                out.push(' ');
            }
        }
        Block::Html { raw } => {
            out.push_str(raw);
            out.push(' ');
        }
        Block::Rule => {}
    }
}

fn words(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: corpus_probe <dir>");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("cannot read {dir}");
        return;
    };
    let (mut total, mut bad) = (0usize, 0usize);
    for e in entries.flatten() {
        let Ok(src) = std::fs::read_to_string(e.path()) else { continue };
        total += 1;
        let tree = parse_full(&src);
        let mut rendered = String::new();
        for TopBlock { block, .. } in &tree.blocks {
            collect(block, &mut rendered);
        }
        let shown = words(&rendered);
        let lost: Vec<String> = words(&src).difference(&shown).cloned().collect();
        if !lost.is_empty() {
            bad += 1;
            if bad <= 15 {
                let preview: String = src.chars().take(200).collect();
                println!(
                    "--- LOST {:?}\n    {}\n",
                    &lost[..lost.len().min(8)],
                    preview.replace('\n', "\\n")
                );
            }
        }
    }
    println!("\nchecked {total} documents, {bad} lost words");
}
