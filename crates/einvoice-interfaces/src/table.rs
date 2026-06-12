//! Shared aligned-table rendering for the CLI reports.
//!
//! One generic helper builds a space-padded, newline-terminated table from a
//! header and rows. Column widths are the widest cell (header included); the last
//! column is not padded, so no trailing whitespace is emitted. Callers append
//! their own summary/legend line. Used by [`crate::analysis`] and [`crate::keys`]
//! so the padding logic lives in one place.

/// Renders `rows` under `header` as a space-padded, newline-terminated table.
pub(crate) fn aligned<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> String {
    let mut widths = header.map(str::len);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }

    let write_row = |out: &mut String, cells: &[String; N]| {
        for (i, (cell, &w)) in cells.iter().zip(&widths).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            // Pad every column but the last so no trailing whitespace is emitted.
            if i + 1 < N {
                for _ in cell.len()..w {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    };

    let mut out = String::new();
    write_row(&mut out, &header.map(String::from));
    write_row(&mut out, &std::array::from_fn(|i| "-".repeat(widths[i])));
    for row in rows {
        write_row(&mut out, row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aligned_pads_to_column_width_without_trailing_space() {
        let rows = [
            ["a".to_string(), "long-value".to_string()],
            ["bb".to_string(), "x".to_string()],
        ];
        let out = aligned(["H1", "H2"], &rows);
        let lines: Vec<&str> = out.lines().collect();
        // Header, rule, then two data rows.
        assert_eq!(lines.len(), 4);
        // First column padded to width 2 ("bb"); last column never padded.
        assert_eq!(lines[2], "a   long-value");
        assert_eq!(lines[3], "bb  x");
        for line in lines {
            assert_eq!(line, line.trim_end(), "no trailing whitespace");
        }
    }
}
