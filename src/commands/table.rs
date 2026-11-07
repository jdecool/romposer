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

/// PHP `wordwrap($text, $width, "\n", true)`, which Symfony applies to cells over a
/// column's max width.
pub fn wordwrap(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut lines = Vec::new();
    let (mut laststart, mut lastspace) = (0usize, 0usize);
    let mut current = 0;
    while current < chars.len() {
        let c = chars[current];
        if c == '\n' {
            lines.push(chars[laststart..current].iter().collect());
            laststart = current + 1;
            lastspace = laststart;
        } else if c == ' ' {
            if current - laststart >= width {
                lines.push(chars[laststart..current].iter().collect());
                laststart = current + 1;
            }
            lastspace = current;
        } else if current - laststart >= width && laststart >= lastspace {
            lines.push(chars[laststart..current].iter().collect());
            laststart = current;
            lastspace = current;
        } else if current - laststart >= width && laststart < lastspace {
            lines.push(chars[laststart..lastspace].iter().collect());
            laststart = lastspace + 1;
            lastspace = laststart;
        }
        current += 1;
    }
    lines.push(chars[laststart..].iter().collect());
    lines
}

/// Symfony `Table` in its default box style: `headers` (none for a horizontal table),
/// column 1 at least `min_width` wide and wrapped at `max_width` when given.
pub fn box_table(
    headers: Option<&[&str]>,
    rows: &[Vec<String>],
    column1: Option<(usize, usize)>,
) -> Vec<String> {
    let wrap = |c: usize, cell: &str| -> Vec<String> {
        match column1 {
            Some((_, max)) if c == 1 && cell.chars().count() > max => wordwrap(cell, max),
            _ => vec![cell.to_string()],
        }
    };
    // each row becomes one line per wrapped cell line, extra lines with empty other cells
    let expand = |row: &[String]| -> Vec<Vec<String>> {
        let cells: Vec<Vec<String>> = row.iter().enumerate().map(|(c, v)| wrap(c, v)).collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|l| {
                cells
                    .iter()
                    .map(|lines| lines.get(l).cloned().unwrap_or_default())
                    .collect()
            })
            .collect()
    };
    let header_lines: Vec<Vec<String>> = headers
        .map(|h| expand(&h.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
        .unwrap_or_default();
    let body: Vec<Vec<String>> = rows.iter().flat_map(|r| expand(r)).collect();
    let columns = header_lines
        .iter()
        .chain(body.iter())
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            header_lines
                .iter()
                .chain(body.iter())
                .map(|r| r.get(c).map_or(0, |s| s.chars().count()))
                .chain(column1.filter(|_| c == 1).map(|(min, _)| min))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let separator = format!(
        "+{}+",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    );
    // `str_pad` counts bytes, the width characters: multibyte cells pad short as in PHP
    let render = |cells: &[String]| -> String {
        let mut line = String::from("|");
        for (c, width) in widths.iter().enumerate() {
            let cell = cells.get(c).map(String::as_str).unwrap_or("");
            let pad = (width + 2).saturating_sub(cell.len() + 2);
            line.push_str(&format!(" {cell}{} |", " ".repeat(pad)));
        }
        line
    };
    let mut lines = vec![separator.clone()];
    if !header_lines.is_empty() {
        lines.extend(header_lines.iter().map(|r| render(r)));
        lines.push(separator.clone());
    }
    lines.extend(body.iter().map(|r| render(r)));
    lines.push(separator);
    lines
}

/// `├──`/`└──`/`│` tree glyphs as Composer prints them without decoration.
pub fn plain_tree_line(line: &str) -> String {
    line.replace('└', "`-")
        .replace('├', "|-")
        .replace("──", "-")
        .replace('│', "|")
}
