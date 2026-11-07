//! Symfony console tables as Composer renders them without decoration.

/// `Table` with the `compact` style: cells padded to the column width, one space after each.
pub fn compact(headers: Option<&[&str]>, rows: &[Vec<String>]) -> Vec<String> {
    let mut all: Vec<Vec<String>> = Vec::new();
    if let Some(headers) = headers {
        all.push(headers.iter().map(|h| h.to_string()).collect());
    }
    all.extend(rows.iter().cloned());
    let columns = all.iter().map(|r| r.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            all.iter()
                .map(|r| r.get(c).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    all.iter()
        .map(|row| {
            let mut line = String::new();
            for (c, width) in widths.iter().enumerate() {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                line.push_str(cell);
                line.push_str(&" ".repeat(width.saturating_sub(cell.chars().count())));
                line.push(' ');
            }
            line
        })
        .collect()
}

/// `SymfonyStyle::table`: the `symfony-style-guide` style with its dashed separators.
pub fn symfony_style(headers: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    let columns = headers.len();
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .map(|r| r.get(c).map_or(0, |s| s.chars().count()))
                .chain(std::iter::once(headers[c].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let separator = format!(
        " {} ",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let render = |cells: Vec<String>| -> String {
        let mut line = String::from(" ");
        let parts: Vec<String> = widths
            .iter()
            .enumerate()
            .map(|(c, width)| {
                let cell = cells.get(c).map(String::as_str).unwrap_or("");
                format!(
                    " {cell}{} ",
                    " ".repeat(width.saturating_sub(cell.chars().count()))
                )
            })
            .collect();
        line.push_str(&parts.join(" "));
        line.push(' ');
        line
    };
    let mut lines = vec![
        separator.clone(),
        render(headers.iter().map(|h| h.to_string()).collect()),
        separator.clone(),
    ];
    for row in rows {
        lines.push(render(row.clone()));
    }
    lines.push(separator);
    lines.push(String::new());
    lines
}

/// `├──`/`└──`/`│` tree glyphs as Composer prints them without decoration.
pub fn plain_tree_line(line: &str) -> String {
    line.replace('└', "`-")
        .replace('├', "|-")
        .replace("──", "-")
        .replace('│', "|")
}
