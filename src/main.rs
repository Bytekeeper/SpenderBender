use ahash::*;
use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use csv::ReaderBuilder;
use num_format::{parsing::ParseFormatted, Locale};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::PathBuf;
use time::Date;

#[derive(Parser)]
struct Args {
    /// CSV File to import
    file: PathBuf,
    /// Group mapping file
    #[arg(short, long)]
    groups: Option<PathBuf>,
    /// Input CSV specification
    #[arg(short = 'i', long, alias = "ff")]
    file_format: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
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

const CSV_DATE_FORMAT: &[time::format_description::FormatItem] =
    time::macros::format_description!("[year]-[month]-[day]");

/// Format used for internal database (not yet implemented)
#[derive(Debug, Deserialize, Serialize)]
struct Record<'r> {
    #[serde(serialize_with = "ser_date", deserialize_with = "deser_date")]
    date: Date,
    party: &'r str,
    description: &'r str,
    amount: f64,
}

fn ser_date<S>(date: &Date, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(
        &date
            .format(&CSV_DATE_FORMAT)
            .map_err(|e| serde::ser::Error::custom(e))?,
    )
}

fn deser_date<'de, D>(d: D) -> Result<Date, D::Error>
where
    D: Deserializer<'de>,
{
    struct FieldVisitor;
    use serde::de;
    use std::fmt;
    impl<'de> de::Visitor<'de> for FieldVisitor {
        type Value = Date;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("YYYY-MM-DD")
        }

        fn visit_str<E>(self, value: &str) -> Result<Date, E>
        where
            E: de::Error,
        {
            Date::parse(value, &CSV_DATE_FORMAT).map_err(|e| de::Error::custom(e))
        }
    }
    d.deserialize_str(FieldVisitor)
}

fn import(config: ImportConfig, mut taker: impl FnMut(Record) -> ()) -> Result<()> {
    let date_format = time::format_description::parse(&config.date_format)?;
    let number_locale = config
        .number_locale
        .map(|locale| Locale::from_name(locale))
        .transpose()?
        .unwrap_or(Locale::en);

    let args: Vec<String> = std::env::args().collect();
    let mut rdr = ReaderBuilder::new()
        .delimiter(b';')
        .flexible(true)
        .has_headers(false)
        .from_path(&args[1])?;
    let records = rdr.into_byte_records();
    let mut records = records.skip(4);
    let header = records.next().ok_or(anyhow!(""))??;
    let field_matchers: Vec<_> = config
        .map
        .iter()
        .flat_map(|(regex, field)| Regex::new(regex).map(|regex| (regex, field)))
        .collect();
    let mut headers: Vec<_> = header
        .iter()
        .enumerate()
        .flat_map(|(i, hdr)| {
            field_matchers
                .iter()
                .find(|(regex, _)| regex.is_match(&String::from_utf8_lossy(hdr)))
                .map(|(_, field)| (i, field))
        })
        .collect();
    // eprintln!("{headers:?}");
    for result in records {
        let result = result?;
        let mut date = None;
        let mut party = None;
        let mut amount = None;
        let mut description = "".to_string();
        for (index, field) in headers.iter() {
            let value = String::from_utf8_lossy(
                result
                    .get(*index)
                    .ok_or_else(|| anyhow!("Not enough data columns"))?,
            );
            match field.as_str() {
                "date" => {
                    date = Some(Date::parse(&value, &date_format).with_context(|| {
                        format!("Parsing '{}' at '{:?}'", value, result.position())
                    })?)
                }
                "party" => party = Some(value.to_string()),
                "amount" => {
                    let mut x = value.splitn(2, number_locale.decimal());
                    let int = x.next().ok_or_else(|| anyhow!("Invalid number"))?;
                    let fract = x.next();
                    let mut result =
                        int.parse_formatted::<_, i64>(&number_locale)
                            .with_context(|| {
                                format!("Parsing '{}' at {:?}", value, result.position())
                            })? as f64;
                    if let Some(fract) = fract {
                        result +=
                            fract.parse::<u64>()? as f64 * 10.0_f64.powf(-(fract.len() as f64));
                    }
                    amount = Some(result)
                }
                "description" => description = value.to_string(),
                _ => unreachable!("Field '{}' does not exist", field),
            }
        }
        let Some(date) = date else { bail!("Date missing") };
        let Some(party) = &party else { bail!("Party missing") };
        let Some(amount) = amount else { bail!("Amount missing") };
        let record = Record {
            date,
            party,
            amount,
            description: &description,
        };
        taker(record);
        // eprintln!("{record:?}");
    }
    Ok(())
}

struct Groups {
    group_matchers: Vec<(Regex, String)>,
    groups: AHashMap<String, f64>,
}

impl Groups {
    fn new(config: GroupConfig) -> Result<Self> {
        let group_matchers = config
            .parties
            .iter()
            .flat_map(|(regex, group)| Regex::new(regex).map(|regex| (regex, group.clone())))
            .collect();
        Ok(Self {
            groups: AHashMap::new(),
            group_matchers,
        })
    }

    fn push(&mut self, record: Record<'_>) {
        let mut hit = true;
        let key = record.party.to_string().to_lowercase();
        let key = self
            .group_matchers
            .iter()
            .find(|(regex, _)| regex.is_match(&key))
            .map(|(_, group)| group)
            .unwrap_or_else(|| {
                hit = false;
                &key
            });
        *self.groups.entry(key.clone()).or_insert_with(|| {
            if !hit {
                eprintln!("No group mapping found for '{}'", key);
            }
            0.0
        }) += record.amount;
    }

    fn print(self) -> Result<()> {
        let mut res: Vec<_> = self.groups.into_iter().collect();
        res.sort_by_key(|(group, amount)| ordered_float::OrderedFloat(*amount));
        for (group, amount) in res {
            println!("{:10.2} {}", amount, group);
        }
        Ok(())
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

    let import_config: ImportConfig = args
        .file_format
        .map(|f| std::fs::read(f))
        .transpose()?
        .map(|f| toml::from_slice(&f))
        .transpose()?
        .unwrap_or_else(|| ImportConfig::default());
    let group_config: GroupConfig = args
        .groups
        .map(|f| std::fs::read(f))
        .transpose()?
        .map(|f| toml::from_slice(&f))
        .transpose()?
        .unwrap_or_else(|| GroupConfig::default());
    let mut groups = Groups::new(group_config)?;
    import(import_config, |it| groups.push(it))?;
    groups.print();
    Ok(())
}
