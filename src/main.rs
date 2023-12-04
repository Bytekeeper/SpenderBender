use ahash::*;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, NaiveDate};
use clap::Parser;
use csv::ReaderBuilder;
use num_format::{parsing::ParseFormatted, Locale};
use regex::Regex;
use rust_xlsxwriter::{Format, Workbook, XlsxColor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{Display, Error, Formatter};
use std::path::PathBuf;
use tokio::runtime::Runtime;
use warp::Filter;

#[derive(PartialEq, Eq, Hash, Copy, Clone, Serialize)]
struct MonthYear {
    month: u32,
    year: i32,
}

impl Display for MonthYear {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        write!(f, "{} {}", self.year, self.month)
    }
}

impl PartialOrd for MonthYear {
    fn partial_cmp(&self, rhs: &Self) -> Option<Ordering> {
        Some(self.cmp(&rhs))
    }
}

impl Ord for MonthYear {
    fn cmp(&self, rhs: &Self) -> Ordering {
        self.year
            .cmp(&rhs.year)
            .then_with(|| (self.month as u8).cmp(&(rhs.month as u8)))
    }
}

impl From<NaiveDate> for MonthYear {
    fn from(date: NaiveDate) -> Self {
        Self {
            month: date.month(),
            year: date.year(),
        }
    }
}

#[derive(Parser)]
struct Args {
    /// CSV File to import
    file: PathBuf,
    /// Group mapping file
    #[arg(short, long)]
    groups: Option<PathBuf>,
    /// Input CSV specification
    #[arg(short = 'i', long, alias = "ff")]
    file_format: PathBuf,
    #[arg(short = 's', long)]
    graph: bool,
}

#[derive(Debug, Deserialize)]
struct ImportConfig {
    skip: Option<usize>,
    date_format: String,
    number_locale: Option<String>,
    map: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct GroupConfig {
    parties: BTreeMap<String, String>,
}

const CSV_DATE_FORMAT: &str = "%Y-%m-%d";

/// Format used for internal database (not yet implemented)
#[derive(Debug, Deserialize, Serialize)]
struct Record<'r> {
    #[serde(serialize_with = "ser_date", deserialize_with = "deser_date")]
    date: NaiveDate,
    party1: &'r str,
    party2: &'r str,
    description: &'r str,
    amount: f64,
}

fn ser_date<S>(date: &NaiveDate, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&date.format(&CSV_DATE_FORMAT).to_string())
}

fn deser_date<'de, D>(d: D) -> Result<NaiveDate, D::Error>
where
    D: Deserializer<'de>,
{
    struct FieldVisitor;
    use serde::de;
    use std::fmt;
    impl<'de> de::Visitor<'de> for FieldVisitor {
        type Value = NaiveDate;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("YYYY-MM-DD")
        }

        fn visit_str<E>(self, value: &str) -> Result<NaiveDate, E>
        where
            E: de::Error,
        {
            NaiveDate::parse_from_str(value, &CSV_DATE_FORMAT).map_err(|e| de::Error::custom(e))
        }
    }
    d.deserialize_str(FieldVisitor)
}

fn import(file: PathBuf, config: ImportConfig, mut taker: impl FnMut(Record) -> ()) -> Result<()> {
    let date_format = &config.date_format;
    let number_locale = config
        .number_locale
        .map(|locale| Locale::from_name(locale))
        .transpose()?
        .unwrap_or(Locale::en);

    let rdr = ReaderBuilder::new()
        .delimiter(b';')
        .flexible(true)
        .has_headers(false)
        .from_path(file)?;
    let records = rdr.into_byte_records();
    let mut records = records.skip(config.skip.unwrap_or(0));
    let header = records.next().ok_or(anyhow!(""))??;
    let field_matchers: Vec<_> = config
        .map
        .iter()
        .flat_map(|(regex, field)| Regex::new(regex).map(|regex| (regex, field)))
        .collect();
    let headers: Vec<_> = header
        .iter()
        .enumerate()
        .flat_map(|(i, hdr)| {
            field_matchers
                .iter()
                .find(|(regex, _)| regex.is_match(&String::from_utf8_lossy(hdr)))
                .map(|(_, field)| (i, field))
        })
        .collect();
    if headers.len() != config.map.len() {
        eprintln!(
            "Headers configured: {:?}, headers actually found: {:?}",
            config.map.keys().collect::<Vec<_>>(),
            headers
        );
    }
    eprintln!("{headers:?}");
    for result in records {
        let result = result?;
        let mut date = None;
        let mut party1 = None;
        let mut party2 = None;
        let mut amount = None;
        let mut description = "".to_string();
        for (index, field) in headers.iter() {
            let value = encoding_rs::UTF_8
                .decode_without_bom_handling(
                    result
                        .get(*index)
                        .ok_or_else(|| anyhow!("Not enough data columns"))?,
                )
                .0;
            match field.as_str() {
                "date" => {
                    date = Some(
                        NaiveDate::parse_from_str(&value, &date_format).with_context(|| {
                            format!(
                                "Parsing '{}' at '{:?}' - is the format '{:?}' correct?",
                                value,
                                result.position(),
                                date_format
                            )
                        })?,
                    )
                }
                "party1" => party1 = Some(value.to_string()),
                "party2" => party2 = Some(value.to_string()),
                "amount" => {
                    let x = value
                        .split_once(number_locale.decimal())
                        .unwrap_or_else(|| (&value, "0"));
                    let int = x.0;
                    let fract = x.1.split_once(' ').map(|(r, _)| r).unwrap_or(x.1);
                    let mut result =
                        int.parse_formatted::<_, i64>(&number_locale)
                            .with_context(|| {
                                format!("Parsing '{}' at {:?}", value, result.position())
                            })? as f64;
                    result += fract.parse::<u64>()? as f64 * 10.0_f64.powf(-(fract.len() as f64));
                    amount = Some(result)
                }
                "description" => description = value.to_string(),
                "party" => {
                    party1 = Some(value.to_string());
                    party2 = Some(value.to_string());
                }
                _ => unreachable!("Field '{}' does not exist", field),
            }
        }
        let Some(date) = date else {
            bail!("Date missing in '{:?}'", result)
        };
        let Some(party1) = &party1 else {
            bail!("Party 1 missing")
        };
        let Some(party2) = &party2 else {
            bail!("Party 2 missing")
        };
        let Some(amount) = amount else {
            bail!("Amount missing")
        };
        let record = Record {
            date,
            party1,
            party2,
            amount,
            description: &description,
        };
        taker(record);
        // eprintln!("{record:?}");
    }
    Ok(())
}

#[derive(Serialize)]
struct Aggregate {
    start: NaiveDate,
    end: NaiveDate,
    stats_summary: Vec<(String, f64)>,
    stats_monthly: Vec<(MonthYear, Vec<(String, f64)>)>,
    stats_grouped: Vec<(String, Vec<(MonthYear, f64)>)>,
}

struct Groups {
    group_matchers: Vec<(Regex, String)>,
    stats_summary: AHashMap<String, f64>,
    stats_monthly: AHashMap<MonthYear, AHashMap<String, f64>>,
    start: NaiveDate,
    end: NaiveDate,
}

impl Groups {
    fn new(config: GroupConfig) -> Result<Self> {
        let group_matchers = config
            .parties
            .iter()
            .flat_map(|(regex, group)| Regex::new(regex).map(|regex| (regex, group.clone())))
            .collect();
        Ok(Self {
            stats_summary: AHashMap::new(),
            stats_monthly: AHashMap::new(),
            group_matchers,
            start: NaiveDate::MAX,
            end: NaiveDate::MIN,
        })
    }

    fn push(&mut self, record: Record<'_>) {
        let mut hit = true;
        let key = if record.amount < 0.0 {
            record.party2.to_string().to_lowercase()
        } else {
            record.party1.to_string().to_lowercase()
        };
        let key = self
            .group_matchers
            .iter()
            .find(|(regex, _)| regex.is_match(&key))
            .map(|(_, group)| group)
            .unwrap_or_else(|| {
                hit = false;
                &key
            });
        *self.stats_summary.entry(key.clone()).or_insert_with(|| {
            if !hit {
                eprintln!("No group mapping found for '{}'", key);
            }
            0.0
        }) += record.amount;
        *self
            .stats_monthly
            .entry(record.date.into())
            .or_insert_with(AHashMap::new)
            .entry(key.clone())
            .or_insert(0.0) += record.amount;
        self.start = self.start.min(record.date);
        self.end = self.end.max(record.date);
    }

    fn aggregate(self) -> Result<Aggregate> {
        let mut stats_summary: Vec<_> = self.stats_summary.into_iter().collect();
        stats_summary.sort_by_key(|(_, amount)| ordered_float::OrderedFloat(*amount));

        let mut stats_monthly: Vec<_> = self
            .stats_monthly
            .iter()
            .map(|(m_y, e)| {
                let mut entries: Vec<_> = e.clone().into_iter().collect();
                entries.sort_by_key(|(_, amount)| ordered_float::OrderedFloat(-amount.abs()));
                entries.truncate(20);

                (*m_y, entries)
            })
            .collect();
        stats_monthly.sort_by_key(|(m_y, _)| *m_y);
        let stats_grouped: Vec<_> = stats_summary
            .iter()
            .map(|(g, _)| {
                let values: Vec<_> = self
                    .stats_monthly
                    .iter()
                    .map(|(m_y, v)| (*m_y, v.get(g).cloned().unwrap_or(0.0)))
                    .collect();
                (g.clone(), values)
            })
            .collect();

        Ok(Aggregate {
            start: self.start,
            end: self.end,
            stats_summary,
            stats_monthly,
            stats_grouped,
        })
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    // let mut wtr = csv::WriterBuilder::new().from_path("tmp.csv")?;
    // wtr.serialize(Record {
    //     date: Date::from_iso_week_date(2022, 5, Wednesday)?,
    //     amount: 0.0,
    //     description: "",
    //     party: "",
    // })?;
    // wtr.flush()?;

    let import_config: Result<ImportConfig, _> =
        toml::from_str(std::str::from_utf8(&std::fs::read(args.file_format)?)?);
    let import_config = import_config?;
    let group_config: GroupConfig = args
        .groups
        .map::<anyhow::Result<GroupConfig>, _>(|f| {
            Ok(toml::from_str(std::str::from_utf8(&std::fs::read(f)?)?)?)
        })
        .transpose()?
        .unwrap_or_else(|| GroupConfig::default());
    let mut groups = Groups::new(group_config)?;
    import(args.file, import_config, |it| groups.push(it))?;
    let result = groups.aggregate()?;
    if args.graph {
        let rt = Runtime::new()?;
        let mut rng = oorandom::Rand64::new(std::time::UNIX_EPOCH.elapsed()?.as_nanos());
        let prefix = rng.rand_u64().to_string();
        println!("Hosting web server on http://127.0.0.1:3030/{}/", prefix);
        rt.block_on(async {
            let data = serde_json::to_string(&result)?;
            let data = warp::path!("data.json").map(move || data.clone());
            let html =
                warp::path::end().map(|| warp::reply::html(include_str!("../res/index.html")));
            let content = warp::path(prefix).and(html.or(data));
            let pure_css = warp::path!("pure-min.css").map(|| include_str!("../res/pure-min.css"));
            let chart_js = warp::path!("chart.js").map(|| include_str!("../res/chart.js"));
            warp::serve(content.or(pure_css).or(chart_js))
                .run(([127, 0, 0, 1], 3030))
                .await;
            Ok::<(), anyhow::Error>(())
        })?;
    } else {
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet().set_name("Summary")?;
        let currency_format = Format::new().set_num_format("#,##0.00 [$€];[RED]-#,##0.00 [$€]");
        let month_format = Format::new()
            .set_bold()
            .set_font_color(XlsxColor::Blue)
            .set_font_size(20);
        worksheet.set_column_format(0, &currency_format)?;
        worksheet.set_column_format(1, &currency_format)?;

        let days = (result.end - result.start).num_days();
        let month_factor = 30.0 / days as f64;
        println!(
            "Summary of spending and revenue from {} to {} ({} days)",
            result.start, result.end, days
        );
        worksheet.write_string(
            0,
            0,
            &format!(
                "Summary of spending and revenue from {} to {} ({} days)",
                result.start, result.end, days
            ),
        )?;
        let mut row = 1;
        for (group, amount) in result.stats_summary {
            println!(
                "{:10.2} ({:10.2} / month) {}",
                amount,
                amount * month_factor,
                group
            );
            worksheet.write_number(row, 0, amount)?;
            worksheet.write_number(row, 1, amount * month_factor)?;
            worksheet.write_string(row, 2, &group)?;
            row += 1;
        }
        worksheet.autofit();
        let worksheet = workbook.add_worksheet().set_name("Monthly Summary")?;
        worksheet.set_column_format(0, &currency_format)?;
        row = 0;
        for (month, groups) in result.stats_monthly {
            worksheet.write_string_with_format(row, 0, &month.to_string(), &month_format)?;
            worksheet.set_row_height(row, 24)?;
            row += 1;
            println!("{month}");
            for (group, amount) in groups.iter().filter(|(_, a)| *a < 0.0) {
                println!("{:10.2} {}", amount, group);
                worksheet.write_number(row, 0, *amount)?;
                worksheet.write_string(row, 1, group)?;
                row += 1;
            }
            for (group, amount) in groups.iter().filter(|(_, a)| *a >= 0.0) {
                println!("{:10.2} {}", amount, group);
                worksheet.write_number(row, 0, *amount)?;
                worksheet.write_string(row, 1, group)?;
                row += 1;
            }
            println!();
            row += 1;
        }
        worksheet.autofit();
        workbook.save("report.xlsx")?;
    }
    Ok(())
}
