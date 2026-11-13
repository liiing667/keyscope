//! Batch validation of providers and parameters from a CSV file.
//!
//! The CSV file starts with a header line whose first cell is `provider` and
//! whose remaining cells are parameter columns. Each data row maps its cells
//! positionally onto the parameters of the provider named in the first cell.
//!
//! Every row is validated independently. A bad row never aborts the run: it is
//! reported as an error and the remaining rows keep executing. Output only
//! ever contains the provider name, row numbers and high-level summaries, so
//! the parameters read from the file are never printed back.

use std::fs;
use std::path::Path;

use keyscope::config::Config;
use keyscope::providers::Provider;
use service_policy_kit::runner::{RunOptions, RunnerReport};

/// Outcome of validating a single CSV row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    /// The policy engine validated the row successfully.
    Success,
    /// The policy engine ran but the policy checks did not pass.
    Failure,
    /// The row could not be validated at all.
    Error,
}

impl RowStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

/// The sanitized result of validating one CSV data row.
#[derive(Debug, Clone)]
pub struct RowReport {
    /// Stable, 1-based row number within the CSV file (header is row 1).
    pub row: usize,
    /// Provider taken from the first cell of the row.
    pub provider: String,
    /// Validation outcome.
    pub status: RowStatus,
    /// Short, parameter-free summary of the outcome.
    pub summary: String,
}

/// Aggregate result of validating every row in a CSV file.
#[derive(Debug, Clone)]
pub struct CsvReport {
    /// One entry per non-empty data row, in file order.
    pub rows: Vec<RowReport>,
}

impl CsvReport {
    #[must_use]
    pub fn success_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == RowStatus::Success)
            .count()
    }

    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == RowStatus::Failure)
            .count()
    }

    #[must_use]
    pub fn error_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == RowStatus::Error)
            .count()
    }

    /// `true` only when every row succeeded.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.failure_count() == 0 && self.error_count() == 0
    }

    /// Render the report for the stdout reporter.
    #[must_use]
    pub fn to_stdout(&self) -> String {
        let mut output = String::new();
        for row in &self.rows {
            output.push_str(&format!("row {} ", row.row));
            if !row.provider.is_empty() {
                output.push_str(&format!("{} ", row.provider));
            }
            output.push_str(&format!("{}: {}\n", row.status.label(), row.summary));
        }

        output.push_str(&format!(
            "\nProcessed {} row(s)\n\nSuccess: {}\nFailure: {}\n  Error: {}",
            self.rows.len(),
            self.success_count(),
            self.failure_count(),
            self.error_count()
        ));
        output
    }

    /// Render the report as pretty JSON without relying on a JSON dependency.
    #[must_use]
    pub fn to_json(&self) -> String {
        let entries = self
            .rows
            .iter()
            .map(|row| {
                format!(
                    "  {{\n    \"row\": {},\n    \"provider\": \"{}\",\n    \"status\": \"{}\",\n    \
                     \"summary\": \"{}\"\n  }}",
                    row.row,
                    escape_json(&row.provider),
                    row.status.label(),
                    escape_json(&row.summary)
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");

        format!("[\n{entries}\n]")
    }
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Read and validate every row of `csv_path`.
///
/// # Errors
///
/// Returns an error when the file cannot be read or does not contain a valid
/// header. Per-row problems are collected into the returned [`CsvReport`]
/// instead of aborting the batch.
pub fn run_csv(
    config: &Config,
    csv_path: &Path,
    dry_run: bool,
    flip: bool,
    verbose: bool,
) -> anyhow::Result<CsvReport> {
    let content = fs::read_to_string(csv_path)?;
    let mut lines = content.lines().peekable();

    let header = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("csv file is empty"))?;
    let header_cells = parse_csv_line(header);
    if header_cells.first().map(|cell| cell.trim()) != Some("provider") {
        return Err(anyhow::anyhow!(
            "csv file must start with a 'provider' header column"
        ));
    }

    let param_columns = header_cells.len().saturating_sub(1);

    let opts = RunOptions::build(
        if dry_run {
            Some("example".to_string())
        } else {
            None
        },
        flip,
        None,
        verbose,
    );

    let mut rows = Vec::new();

    // The header occupies physical line 1; data rows start at line 2.
    for (line_index, line) in lines.enumerate() {
        let row_number = line_index + 2;
        if line.trim().is_empty() {
            continue;
        }

        let cells = parse_csv_line(line);
        let provider_name = cells.first().map_or("", String::as_str).trim().to_string();

        let params: Vec<String> = cells
            .iter()
            .skip(1)
            .take(param_columns)
            .map(std::string::ToString::to_string)
            .collect();

        let report = validate_row(config, &provider_name, &params, row_number, &opts);
        rows.push(report);
    }

    if rows.is_empty() {
        return Err(anyhow::anyhow!("csv file contains no data rows"));
    }

    Ok(CsvReport { rows })
}

fn validate_row(
    config: &Config,
    provider_name: &str,
    params: &[String],
    row_number: usize,
    opts: &RunOptions,
) -> RowReport {
    if provider_name.is_empty() {
        return RowReport {
            row: row_number,
            provider: String::new(),
            status: RowStatus::Error,
            summary: "missing provider name".to_string(),
        };
    }

    let provider = match config.get_providers(provider_name) {
        Ok(provider) => provider,
        Err(_) => {
            return RowReport {
                row: row_number,
                provider: provider_name.to_string(),
                status: RowStatus::Error,
                summary: "unknown provider".to_string(),
            };
        }
    };

    if params.iter().all(|param| param.trim().is_empty()) {
        return RowReport {
            row: row_number,
            provider: provider_name.to_string(),
            status: RowStatus::Error,
            summary: "no parameters provided".to_string(),
        };
    }

    match provider.key_validate_with_opts(params, opts) {
        Ok(report) => summarize_report(provider, row_number, report),
        Err(_) => RowReport {
            row: row_number,
            provider: provider_name.to_string(),
            status: RowStatus::Error,
            summary: "parameters do not satisfy provider requirements".to_string(),
        },
    }
}

fn summarize_report(provider: &Provider, row_number: usize, report: RunnerReport) -> RowReport {
    let mut status = if report.ok {
        RowStatus::Success
    } else {
        RowStatus::Failure
    };
    let mut summary = if report.ok {
        "validation passed".to_string()
    } else {
        "validation failed".to_string()
    };

    // An engine-side transport/execution error is an error rather than a
    // policy failure. Error strings may contain rendered URLs with embedded
    // parameters, so they are deliberately not copied into the summary.
    if report.results.iter().any(|result| result.error.is_some()) {
        status = RowStatus::Error;
        summary = "validation error".to_string();
    } else if !report.ok {
        let subjects = report
            .results
            .iter()
            .flat_map(|result| result.violations.iter())
            .map(|violation| violation.subject.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if !subjects.is_empty() {
            summary = format!("validation failed: {subjects}");
        }
    }

    RowReport {
        row: row_number,
        provider: provider.name().to_string(),
        status,
        summary,
    }
}

/// Parse a single CSV line, honoring double-quoted fields with embedded commas
/// and escaped quotes (`""`).
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    cell.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cell.push(ch);
            }
        } else {
            match ch {
                '"' => in_quotes = true,
                ',' => {
                    cells.push(std::mem::take(&mut cell));
                }
                _ => cell.push(ch),
            }
        }
    }

    cells.push(cell);
    cells
}

#[cfg(test)]
mod tests {
    use super::parse_csv_line;

    #[test]
    fn parses_simple_csv_line() {
        assert_eq!(
            parse_csv_line("provider,key1,key2"),
            vec!["provider", "key1", "key2"]
        );
    }

    #[test]
    fn parses_empty_and_quoted_cells() {
        assert_eq!(
            parse_csv_line("twilio,tw-key1,,\"a,b\",\"he said \"\"hi\"\"\""),
            vec!["twilio", "tw-key1", "", "a,b", "he said \"hi\""]
        );
    }
}
