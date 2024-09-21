use chrono::{DateTime, Duration, Utc};
use dotenv::dotenv;
use env_logger;
use log::{error, info, warn};
use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::channel::Message;
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use urlencoding::encode;

// ----------------------------
// Data Structures
// ----------------------------

#[derive(Deserialize, Debug)]
struct WikiSummary {
    extract: String,
    thumbnail: Option<Thumbnail>,
    originalimage: Option<OriginalImage>,
}

#[derive(Deserialize, Debug)]
struct Thumbnail {
    source: String,
}

#[derive(Deserialize, Debug)]
struct OriginalImage {
    source: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BugLogEntry {
    bug_name: String,
    wiki_url: String,
    timestamp: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct CurrentBug {
    bug_name: String,
    wiki_url: String,
    timestamp: DateTime<Utc>,
}

// ----------------------------
// TypeMap Keys
// ----------------------------

struct HiveDataKey;

impl TypeMapKey for HiveDataKey {
    type Value = HashMap<String, String>;
}

struct WikiCacheKey;

impl TypeMapKey for WikiCacheKey {
    type Value = tokio::sync::Mutex<HashMap<String, (String, Option<String>)>>;
}

// ----------------------------
// Helper Functions
// ----------------------------

async fn fetch_wikipedia_summary(
    client: &Client,
    bug_name: &str,
    cache: &tokio::sync::Mutex<HashMap<String, (String, Option<String>)>>,
) -> Result<(String, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    // Check cache
    {
        let cache = cache.lock().await;
        if let Some(data) = cache.get(bug_name) {
            info!("Cache hit for bug: {}", bug_name);
            return Ok(data.clone());
        }
    }

    // Fetch from Wikipedia API
    let encoded_name = encode(bug_name);
    let url = format!(
        "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
        encoded_name
    );

    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        return Err(format!(
            "Failed to fetch data from Wikipedia for '{}'. Status: {}",
            bug_name,
            resp.status()
        )
        .into());
    }

    let summary: WikiSummary = resp.json().await?;

    // Extract image URL as Option<String>
    let image_url = summary
        .originalimage
        .map(|img| img.source)
        .or(summary.thumbnail.map(|thumb| thumb.source));

    let data = (summary.extract, image_url);

    // Update cache
    {
        let mut cache = cache.lock().await;
        cache.insert(bug_name.to_string(), data.clone());
        info!("Cache updated for bug: {}", bug_name);
    }

    Ok(data)
}

async fn load_hive_data(
    path: &str,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut file = File::open(path).await?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let bugs: HashMap<String, String> = serde_json::from_str(&contents)?;
    Ok(bugs)
}

async fn read_bug_log(
    path: &str,
) -> Result<Vec<BugLogEntry>, Box<dyn std::error::Error + Send + Sync>> {
    if !Path::new(path).exists() {
        info!("bug_log.json does not exist. Initializing a new log.");
        return Ok(Vec::new());
    }

    let contents = fs::read_to_string(path).await?;
    match serde_json::from_str::<Vec<BugLogEntry>>(&contents) {
        Ok(log) => Ok(log),
        Err(e) => {
            error!(
                "Error parsing bug_log.json. Initializing a new log. Details: {}",
                e
            );
            Ok(Vec::new())
        }
    }
}

async fn write_bug_log(
    path: &str,
    log: &Vec<BugLogEntry>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let contents = serde_json::to_string_pretty(log)?;
    let temp_path = format!("{}.tmp", path);

    fs::write(&temp_path, contents).await?;
    fs::rename(&temp_path, path).await?;

    info!("bug_log.json updated successfully.");
    Ok(())
}

async fn read_current_bug(
    path: &str,
) -> Result<CurrentBug, Box<dyn std::error::Error + Send + Sync>> {
    if !Path::new(path).exists() {
        return Err("current_bug.json does not exist.".into());
    }

    let contents = fs::read_to_string(path).await?;
    let current_bug: CurrentBug = serde_json::from_str(&contents)?;
    Ok(current_bug)
}

async fn write_current_bug(
    path: &str,
    current_bug: &CurrentBug,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let contents = serde_json::to_string_pretty(current_bug)?;
    let temp_path = format!("{}.tmp", path);

    fs::write(&temp_path, contents).await?;
    fs::rename(&temp_path, path).await?;

    info!("current_bug.json updated successfully.");
    Ok(())
}

fn get_available_bugs<'a>(
    hive_data: &'a HashMap<String, String>,
    recent_bugs: &HashSet<String>,
) -> Vec<(&'a String, &'a String)> {
    hive_data
        .iter()
        .filter(|(bug_name, _)| !recent_bugs.contains(*bug_name))
        .collect()
}

// ----------------------------
// Event Handler
// ----------------------------

struct Handler {
    http_client: Client,
    log_path: String,
    current_bug_path: String,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: serenity::model::gateway::Ready) {
        info!("{} is connected!", ready.user.name);

        let http = ctx.http.clone();
        let data = ctx.data.clone();
        let log_path = self.log_path.clone();
        let http_client = self.http_client.clone(); // Clone the http_client for the closure

        let bug_channel_id = match env::var("BUG_CHANNEL_ID") {
            Ok(val) => match val.parse::<u64>() {
                Ok(id) => serenity::model::id::ChannelId::new(id),
                Err(_) => {
                    error!("BUG_CHANNEL_ID is not a valid u64.");
                    return;
                }
            },
            Err(_) => {
                error!("BUG_CHANNEL_ID is not set in the environment.");
                return;
            }
        };

        tokio::spawn(async move {
            let interval_duration = Duration::weeks(2)
                .to_std()
                .unwrap_or_else(|_| std::time::Duration::from_secs(14 * 24 * 60 * 60));

            loop {
                let data_read = data.read().await;
                let hive = match data_read.get::<HiveDataKey>() {
                    Some(hive) => hive,
                    None => {
                        error!("Hive data is not available.");
                        break;
                    }
                };

                let wiki_cache_mutex = match data_read.get::<WikiCacheKey>() {
                    Some(mutex) => mutex,
                    None => {
                        error!("Wiki cache mutex not found.");
                        break;
                    }
                };

                let bug_log = match read_bug_log(&log_path).await {
                    Ok(log) => log,
                    Err(e) => {
                        error!("Error reading bug log: {}", e);
                        break;
                    }
                };

                let recent_bugs: HashSet<String> =
                    bug_log.iter().map(|entry| entry.bug_name.clone()).collect();
                let available_bugs = get_available_bugs(&hive, &recent_bugs);

                if available_bugs.is_empty() {
                    error!("No available bugs to select.");
                    break;
                }

                let mut rng = SmallRng::from_entropy();
                let (bug_name, wiki_url) = match available_bugs.choose(&mut rng) {
                    Some(bug) => bug,
                    None => {
                        error!("Failed to select a bug.");
                        break;
                    }
                };

                let (summary, image_url) =
                    match fetch_wikipedia_summary(&http_client, bug_name, wiki_cache_mutex).await {
                        Ok(data) => data,
                        Err(e) => {
                            error!("Error fetching Wikipedia data: {}", e);
                            break;
                        }
                    };

                let mut embed = serenity::builder::CreateEmbed::default()
                    .title(bug_name.as_str())
                    .url(wiki_url.as_str())
                    .description(summary)
                    .color(0x1D82B6);

                if let Some(image) = image_url {
                    embed = embed.image(image);
                }

                let message = serenity::builder::CreateMessage::default().embed(embed);

                if let Err(why) = bug_channel_id.send_message(&http, message).await {
                    error!("Error sending embed: {:?}", why);
                } else {
                    info!("Fortnightly bug sent: {}", bug_name);

                    let new_current_bug = CurrentBug {
                        bug_name: bug_name.to_string(),
                        wiki_url: wiki_url.to_string(),
                        timestamp: Utc::now(),
                    };

                    if let Err(e) = write_current_bug("current_bug.json", &new_current_bug).await {
                        error!("Error writing current_bug.json: {}", e);
                    }

                    let mut updated_log = bug_log;
                    updated_log.push(BugLogEntry {
                        bug_name: bug_name.to_string(),
                        wiki_url: wiki_url.to_string(),
                        timestamp: Utc::now(),
                    });

                    if updated_log.len() >= 26 {
                        // Approximately one year of logs
                        updated_log.clear();
                        info!("Bug log cleared as it reached capacity.");
                    }

                    if let Err(e) = write_bug_log(&log_path, &updated_log).await {
                        error!("Error writing bug log: {}", e);
                    }
                }

                tokio::time::sleep(interval_duration).await;
            }
        });
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let content = msg.content.trim();

        match content.to_lowercase().as_str() {
            "*bug" => {
                let current_bug = match read_current_bug(&self.current_bug_path).await {
                    Ok(bug) => bug,
                    Err(_) => {
                        if let Err(why) = msg
                            .channel_id
                            .say(
                                &ctx.http,
                                "No bug has been selected for this fortnight yet.",
                            )
                            .await
                        {
                            error!("Error sending message: {:?}", why);
                        }
                        return;
                    }
                };

                let data = ctx.data.read().await;
                let wiki_cache_mutex = match data.get::<WikiCacheKey>() {
                    Some(mutex) => mutex,
                    None => {
                        error!("Wiki cache mutex not found.");
                        if let Err(why) = msg
                            .channel_id
                            .say(&ctx.http, "Wiki cache is not available.")
                            .await
                        {
                            error!("Error sending message: {:?}", why);
                        }
                        return;
                    }
                };

                let (summary, image_url) = match fetch_wikipedia_summary(
                    &self.http_client,
                    &current_bug.bug_name,
                    wiki_cache_mutex,
                )
                .await
                {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Error fetching Wikipedia data: {}", e);
                        if let Err(why) = msg
                            .channel_id
                            .say(&ctx.http, "Failed to retrieve data from Wikipedia.")
                            .await
                        {
                            error!("Error sending message: {:?}", why);
                        }
                        return;
                    }
                };

                let mut embed = serenity::builder::CreateEmbed::default()
                    .title(current_bug.bug_name.as_str())
                    .url(current_bug.wiki_url.as_str())
                    .description(summary)
                    .color(0x1D82B6);

                if let Some(image) = image_url {
                    embed = embed.image(image);
                }

                let message = CreateMessage::default().embed(embed);

                if let Err(why) = msg.channel_id.send_message(&ctx.http, message).await {
                    error!("Error sending embed: {:?}", why);
                }
            }
            "*buglog" => {
                let bug_log = match read_bug_log(&self.log_path).await {
                    Ok(log) => log,
                    Err(e) => {
                        error!("Error reading bug log: {}", e);
                        if let Err(why) = msg
                            .channel_id
                            .say(&ctx.http, "Failed to read the bug log.")
                            .await
                        {
                            error!("Error sending message: {:?}", why);
                        }
                        return;
                    }
                };

                if bug_log.is_empty() {
                    if let Err(why) = msg
                        .channel_id
                        .say(&ctx.http, "No bug history available.")
                        .await
                    {
                        error!("Error sending message: {:?}", why);
                    }
                    return;
                }

                let mut log_message = String::from("**Last Bugs Returned:**\n");
                for (index, entry) in bug_log.iter().enumerate() {
                    log_message.push_str(&format!(
                        "{}. **{}** - <{}> at {}\n",
                        index + 1,
                        entry.bug_name,
                        entry.wiki_url,
                        entry.timestamp.format("%Y-%m-%d %H:%M:%S")
                    ));
                }

                if let Err(why) = msg.channel_id.say(&ctx.http, log_message).await {
                    error!("Error sending bug log: {:?}", why);
                }
            }
            "*whenbug" => {
                let current_bug = match read_current_bug(&self.current_bug_path).await {
                    Ok(bug) => bug,
                    Err(_) => {
                        if let Err(why) = msg
                            .channel_id
                            .say(&ctx.http, "No bug has been selected yet.")
                            .await
                        {
                            error!("Error sending message: {:?}", why);
                        }
                        return;
                    }
                };

                let next_bug_time = current_bug.timestamp + Duration::weeks(2);
                let now = Utc::now();
                let duration_until_next_bug = next_bug_time - now;

                if duration_until_next_bug.num_seconds() <= 0 {
                    if let Err(why) = msg
                        .channel_id
                        .say(
                            &ctx.http,
                            "A new bug is being selected or will be selected very soon!",
                        )
                        .await
                    {
                        error!("Error sending message: {:?}", why);
                    }
                    return;
                }

                let days = duration_until_next_bug.num_days();
                let hours = duration_until_next_bug.num_hours() % 24;
                let minutes = duration_until_next_bug.num_minutes() % 60;
                let seconds = duration_until_next_bug.num_seconds() % 60;

                let time_left = format!(
                    "{} day(s), {} hour(s), {} minute(s), {} second(s)",
                    days, hours, minutes, seconds
                );

                let embed = serenity::builder::CreateEmbed::default()
                    .title("Next Bug Day")
                    .description(format!("**{}**", time_left))
                    .color(0xFFD700);

                let message = CreateMessage::default().embed(embed);

                if let Err(why) = msg.channel_id.send_message(&ctx.http, message).await {
                    error!("Error sending !whenBug embed: {:?}", why);
                }
            }
            "*help" => {
                let embed = serenity::builder::CreateEmbed::default()
                    .title("Bug Bot Help")
                    .description(
                        "Here are the available commands:\n\n\
                        **`*bug`** - Displays the current bug of the fortnight.\n\
                        **`*buglog`** - Shows the history of previously selected bugs.\n\
                        **`*whenbug`** - Shows the time remaining until the next bug day.\n\
                        **`*help`** - Displays this help message.",
                    )
                    .color(0x00FF00);

                let message = CreateMessage::default().embed(embed);

                if let Err(why) = msg.channel_id.send_message(&ctx.http, message).await {
                    error!("Error sending help embed: {:?}", why);
                }
            }
            _ => {}
        }
    }
}

// ----------------------------
// Main Function
// ----------------------------

#[tokio::main]
async fn main() {
    dotenv().ok();
    env_logger::init();

    let token = match env::var("DISCORD_TOKEN") {
        Ok(val) => val,
        Err(_) => {
            error!("DISCORD_TOKEN is not set in the environment.");
            return;
        }
    };

    let hive_data = match load_hive_data("hive.json").await {
        Ok(data) => data,
        Err(e) => {
            error!("Error loading hive.json: {}", e);
            return;
        }
    };

    if hive_data.is_empty() {
        warn!("hive.json is empty. Please add bug data.");
        return;
    }

    info!("Loaded {} bugs from hive.json.", hive_data.len());

    let http_client = Client::new();
    let log_path = "bug_log.json".to_string();
    let current_bug_path = "current_bug.json".to_string();
    let wiki_cache = HashMap::new();

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = match serenity::Client::builder(&token, intents)
        .event_handler(Handler {
            http_client: http_client.clone(),
            log_path: log_path.clone(),
            current_bug_path: current_bug_path.clone(),
        })
        .await
    {
        Ok(client) => client,
        Err(e) => {
            error!("Error creating client: {}", e);
            return;
        }
    };

    {
        let mut data = client.data.write().await;
        data.insert::<HiveDataKey>(hive_data);
        data.insert::<WikiCacheKey>(tokio::sync::Mutex::new(wiki_cache));
    }

    info!("Starting the Discord bot...");

    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}
