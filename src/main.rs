use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::TimeDelta;
use chrono::TimeZone;
use chrono::Utc;
use futures::StreamExt;
use seabird::Client;
use seabird::ClientConfig;
use seabird::proto::ChannelSource;
use seabird::proto::CommandEvent;
use seabird::proto::CommandMetadata;
use seabird::proto::StreamEventsRequest;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::ops::RangeInclusive;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
struct BandData {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@time")]
    time: String,
    #[serde(rename = "#text")]
    condition: String,
}

#[derive(Debug, Deserialize)]
struct CalculatedConditions {
    band: Vec<BandData>,
}

#[derive(Debug, Deserialize)]
struct SolarData {
    updated: String,
    calculatedconditions: CalculatedConditions,
}

#[derive(Debug, Deserialize)]
struct Solar {
    solardata: SolarData,
}

struct PossibleBandCondition {
    day: Option<String>,
    night: Option<String>,
}

impl Default for PossibleBandCondition {
    fn default() -> Self {
        Self {
            day: None,
            night: None,
        }
    }
}

struct BandCondition {
    day: String,
    night: String,
}

impl TryFrom<PossibleBandCondition> for BandCondition {
    type Error = anyhow::Error;

    fn try_from(value: PossibleBandCondition) -> Result<Self> {
        Ok(Self {
            day: value.day.ok_or_else(|| anyhow!("missing day value"))?,
            night: value.night.ok_or_else(|| anyhow!("missing night value"))?,
        })
    }
}

fn format_solar_data(data: Solar) -> Result<Vec<String>> {
    let mut possible_bands = HashMap::new();
    for band_data in data.solardata.calculatedconditions.band {
        let condition = possible_bands
            .entry(band_data.name.clone())
            .or_insert_with(|| PossibleBandCondition::default());
        match band_data.time.as_str() {
            "day" => {
                if condition.day.is_some() {
                    bail!("day conditions for band {} already set", band_data.name);
                }

                condition.day = Some(band_data.condition);
            }
            "night" => {
                if condition.night.is_some() {
                    bail!("night conditions for band {} already set", band_data.name);
                }

                condition.night = Some(band_data.condition);
            }
            _ => {
                bail!(
                    "unknown time {} for band {}",
                    band_data.time,
                    band_data.name
                );
            }
        }
    }

    let mut bands = BTreeMap::new();
    for (name, band) in possible_bands {
        let band: BandCondition = band.try_into()?;
        bands.insert(name, band);
    }

    let mut output: Vec<String> = Vec::new();
    output.push(format!("updated {}", data.solardata.updated));

    for (name, band) in bands {
        output.push(format!(
            "{} - day: {}, night: {}",
            name, band.day, band.night
        ));
    }

    Ok(output)
}

async fn fetch_solar_data() -> Result<Solar> {
    let text = reqwest::get("https://www.hamqsl.com/solarxml.php")
        .await?
        .text()
        .await?;

    Ok(serde_xml_rs::from_str(&text)?)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
enum Mode {
    #[serde(rename = "")]
    Unknown,
    Ft4,
    Ft8,
    Ssb,
    Usb,
    Lsb,
    Cw,
    Fm,
    Rtty,
    C4fm,
    Psk31,
    Dstar,
}

impl FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_uppercase().as_str() {
            "FT4" => Ok(Mode::Ft4),
            "FT8" => Ok(Mode::Ft8),
            "LSB" => Ok(Mode::Lsb),
            "USB" => Ok(Mode::Usb),
            "SSB" => Ok(Mode::Ssb),
            "CW" => Ok(Mode::Cw),
            "FM" => Ok(Mode::Fm),
            "RTTY" => Ok(Mode::Rtty),
            "C4FM" => Ok(Mode::C4fm),
            "PSK31" => Ok(Mode::Psk31),
            "DSTAR" => Ok(Mode::Dstar),
            _ => Err(anyhow!("unknown mode \"{value}\"")),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Mode::Ft4 => "FT4",
                Mode::Ft8 => "FT8",
                Mode::Lsb => "LSB",
                Mode::Usb => "USB",
                Mode::Ssb => "SSB",
                Mode::Cw => "CW",
                Mode::Fm => "FM",
                Mode::Rtty => "RTTY",
                Mode::C4fm => "C4FM",
                Mode::Psk31 => "PSK31",
                Mode::Dstar => "DSTAR",
                Mode::Unknown => "unknown",
            }
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParsedActivation {
    activator: String,
    name: String,
    location_desc: String,
    mode: Mode,
    frequency: String,
    spot_time: String,
}

impl ParsedActivation {
    fn try_into_activation(self) -> Result<Activation> {
        Ok(Activation {
            activator: self.activator,
            name: self.name,
            location_desc: self.location_desc,
            mode: self.mode,
            frequency: ((self.frequency.parse::<f64>()? * 1_000.0).floor() as usize).into(),
            spot_time: Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(
                &self.spot_time,
                "%Y-%m-%dT%H:%M:%S",
            )?),
        })
    }
}

#[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
struct Frequency(usize);

impl From<usize> for Frequency {
    fn from(val: usize) -> Self {
        Self(val)
    }
}

impl Frequency {
    fn mhz(&self) -> usize {
        self.0 / 1_000_000
    }
}

impl FromStr for Frequency {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(Self((value.parse::<f64>()? * 1_000.0).floor() as usize))
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let khz = (self.0 % 1_000_000) / 1_000;
        let hz = self.0 % 1_000;
        write!(
            f,
            "{}.{:0<3}{}",
            self.mhz(),
            khz,
            if hz == 500 { ".5" } else { "" }
        )
    }
}

#[derive(Debug)]
struct Activation {
    activator: String,
    name: String,
    location_desc: String,
    mode: Mode,
    frequency: Frequency,
    spot_time: DateTime<Utc>,
}

impl Activation {
    fn age(&self) -> TimeDelta {
        self.spot_time - Utc::now()
    }
}

#[derive(Debug)]
enum Band {
    B20m,
    B40m,
}

impl FromStr for Band {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_lowercase().as_str() {
            "20m" => Ok(Band::B20m),
            "40m" => Ok(Band::B40m),
            _ => Err(anyhow!("unknown band \"{value}\"")),
        }
    }
}

impl fmt::Display for Band {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Band::B20m => "20m",
                Band::B40m => "30m",
            }
        )
    }
}

impl Band {
    fn frequency_range(&self) -> RangeInclusive<Frequency> {
        match self {
            Band::B20m => Frequency(14000000)..=Frequency(14350000),
            Band::B40m => Frequency(7000000)..=Frequency(7300000),
        }
    }
}

async fn fetch_activations() -> Result<Vec<Activation>> {
    reqwest::get("https://api.pota.app/v1/spots")
        .await?
        .json::<Vec<ParsedActivation>>()
        .await?
        .into_iter()
        .map(|a| a.try_into_activation())
        .collect::<Result<Vec<Activation>>>()
}

fn with_reply(command_source: &ChannelSource, message: String) -> String {
    format!(
        "{}{}",
        command_source
            .user
            .as_ref()
            .map(|u| format!("{}: ", u.display_name))
            .unwrap_or_else(|| "".to_string()),
        message
    )
}

async fn most_recent_activation(band: &Band, mode: &Mode) -> Result<Option<Activation>> {
    let activations = fetch_activations().await?;
    for activation in activations {
        if band.frequency_range().contains(&activation.frequency) && &activation.mode == mode {
            return Ok(Some(activation));
        }
    }

    Ok(None)
}

async fn handle_pota_impl(
    client: &mut Client,
    band_str: &str,
    mode: Mode,
    command_source: ChannelSource,
) -> Result<()> {
    let band = match band_str.parse::<Band>() {
        Ok(band) => band,
        Err(_) => {
            client
                .send_message(
                    command_source.channel_id.clone(),
                    with_reply(&command_source, "invalid_band".to_string()),
                    /* tags = */ None,
                )
                .await?;
            return Ok(());
        }
    };

    match most_recent_activation(&band, &mode).await? {
        Some(activation) => {
            let age_string = {
                let seconds = activation.age().num_seconds().abs();
                if seconds > 60 {
                    format!("{}m{}s", seconds / 60, seconds % 60)
                } else {
                    seconds.to_string()
                }
            };

            client
                .send_message(
                    command_source.channel_id.clone(),
                    with_reply(
                        &command_source,
                        format!(
                            "[time:{},age:{}] {}MHz {}, {} - {} ({})",
                            activation.spot_time,
                            age_string,
                            activation.frequency,
                            activation.mode,
                            activation.location_desc,
                            activation.name,
                            activation.activator,
                        ),
                    ),
                    /* tags = */ None,
                )
                .await?;
        }
        None => {
            client
                .send_message(
                    command_source.channel_id.clone(),
                    with_reply(
                        &command_source,
                        format!("no activations found on {} over SSB", band),
                    ),
                    /* tags = */ None,
                )
                .await?;
        }
    }

    Ok(())
}

async fn handle_pota(client: &mut Client, arg: &str, command_source: ChannelSource) -> Result<()> {
    let parts: Vec<_> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [band_str] => {
            handle_pota_impl(client, band_str, Mode::Ssb, command_source).await?;
        }
        [band_str, mode_str] => {
            let mode = match mode_str.parse::<Mode>() {
                Ok(mode) => mode,
                Err(_) => {
                    client
                        .send_message(
                            command_source.channel_id.clone(),
                            format!(
                                "{}invalid mode",
                                command_source
                                    .user
                                    .map(|u| format!("{}: ", u.display_name))
                                    .unwrap_or_else(|| "".to_string())
                            ),
                            /* tags = */ None,
                        )
                        .await?;
                    return Ok(());
                }
            };

            handle_pota_impl(client, band_str, mode, command_source).await?;
        }
        _ => {
            client
                .send_message(
                    command_source.channel_id.clone(),
                    format!(
                        "{}invalid pota command. Usage: pota <band> [mode]",
                        command_source
                            .user
                            .map(|u| format!("{}: ", u.display_name))
                            .unwrap_or_else(|| "".to_string())
                    ),
                    /* tags = */ None,
                )
                .await?;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = env::var("SEABIRD_URL").unwrap_or_else(|_| "https://api.seabird.chat".to_string());
    println!("connecting with URL {}", url);

    let token = env::var("SEABIRD_TOKEN")?;
    let mut client = Client::new(ClientConfig { url, token }).await?;

    let commands = HashMap::from_iter([(
        "bands".to_string(),
        CommandMetadata {
            name: "bands".to_string(),
            short_help: "show HAM RF band conditions".to_string(),
            full_help: "show HAM RF band conditions".to_string(),
        },
    ), (
        "pota".to_string(),
        CommandMetadata {
            name: "pota".to_string(),
            short_help: "find most recent POTA activation".to_string(),
            full_help: "find the most recent Parks on the Air activation. Usage: pota <band> [mode]. Default mode is SSB.".to_string(),
        }
    )]);

    let mut stream = client
        .inner_mut_ref()
        .stream_events(StreamEventsRequest { commands })
        .await?
        .into_inner();

    while let Some(event) = stream.next().await.transpose()? {
        if let Some(seabird::proto::event::Inner::Command(CommandEvent {
            source: Some(command_source),
            command,
            arg,
        })) = event.inner
        {
            if command == "bands" {
                let output = format_solar_data(fetch_solar_data().await?)?;
                client
                    .send_message(
                        command_source.channel_id.clone(),
                        match command_source.user {
                            Some(user) => {
                                format!("{}: current band conditions:", user.display_name)
                            }
                            None => "current band conditions:".to_string(),
                        },
                        /* tags = */ None,
                    )
                    .await?;

                for line in output {
                    client
                        .send_message(
                            command_source.channel_id.clone(),
                            line,
                            /* tags = */ None,
                        )
                        .await?;
                }
            } else if command == "pota" {
                handle_pota(&mut client, &arg, command_source).await?;
            }
        }
    }

    Ok(())
}
