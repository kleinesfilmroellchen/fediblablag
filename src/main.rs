#![doc = include_str!("../README.md")]
#![deny(clippy::all, rustdoc::all, unused, missing_docs)]
#![allow(unused)]

use comrak::Arena;
use comrak::ListStyleType;
use comrak::Options;
use dotenv;
use elefren::prelude::*;
use env_logger;
use fancy_regex::Regex;
use log::debug;
use log::error;
use log::info;
use log::warn;
use serde::Deserialize;
use std::env::var;
use std::env::VarError;
use std::process::exit;
use std::sync::OnceLock;

static SPLIT_POINT: OnceLock<Regex> = OnceLock::new();

fn parse_to_html(input: &str) -> String {
    // html parsing is disabled for now
    input.to_string()

    // comrak::markdown_to_html(input, &comrak_options())
}

fn comrak_options() -> Options {
    let mut conversion_options = Options::default();
    conversion_options.extension.strikethrough = true;
    conversion_options.extension.tagfilter = true;
    conversion_options.extension.table = true;
    conversion_options.extension.tasklist = true;
    conversion_options.extension.footnotes = true;
    conversion_options.extension.description_lists = true;
    conversion_options.extension.front_matter_delimiter = Some("---".to_owned());
    conversion_options.parse.smart = true;
    conversion_options.render.unsafe_ = true;
    conversion_options.render.list_style = ListStyleType::Dash;
    conversion_options.render.escape = true;
    conversion_options.render.width = usize::MAX;
    conversion_options
}

/// User-definable options for a post, specified in the frontmatter.
#[derive(Deserialize, Default, Debug)]
struct PostOptions {
    #[serde(default, rename = "cn")]
    content_notice: Option<String>,
}

fn extract_options_from_frontmatter(input: &str) -> PostOptions {
    let arena = Arena::new();
    let document = comrak::parse_document(&arena, input, &comrak_options());
    let frontmatter = document
        .descendants()
        .filter_map(|element| match &element.data.borrow().value {
            comrak::nodes::NodeValue::FrontMatter(text) => Some(
                // This is a little bit of a hack: remove Carriage Returns because Windows, and then the frontmatter delimiters themselves.
                text.trim()
                    .replace('\r', "")
                    .trim_start_matches("---\n")
                    .trim_end_matches("\n---")
                    .to_string(),
            ),
            _ => None,
        })
        .next()
        .unwrap_or_default();
    serde_yaml::from_str::<PostOptions>(&frontmatter)
        .ok()
        .unwrap_or_default()
}

fn remove_frontmatter(input: &str) -> String {
    let arena = Arena::new();
    let document = comrak::parse_document(&arena, input, &comrak_options());
    for element in document.descendants() {
        if matches!(
            &element.data.borrow().value,
            comrak::nodes::NodeValue::FrontMatter(_)
        ) {
            element.detach();
        }
    }
    let mut output = Vec::new();
    comrak::format_commonmark(document, &comrak_options(), &mut output).unwrap();
    // More hacks: comrak likes to use escaped exclamation marks for no reason.
    String::from_utf8_lossy(&output).replace("\\!", "!")
}

/// Split a piece of text at given indices.
fn split_at_indices<'a>(input: &'a str, split_points: &'_ [usize]) -> Vec<&'a str> {
    let mut current_start = 0;
    let mut elements = Vec::new();
    let mut current_input = input;
    for absolute_point in split_points {
        let next_point = absolute_point - current_start;
        let (segment, remainder) = current_input.split_at(next_point);
        current_start = *absolute_point;
        elements.push(segment);
        current_input = remainder;
    }

    elements.push(current_input);
    elements
}

fn is_under_post_limit(
    text: &str,
    post_number: usize,
    post_count: usize,
    character_limit: usize,
) -> bool {
    let post_count_length = (post_count as f64).log10().ceil() as usize;
    let post_number_length = (post_number as f64).log10().ceil() as usize;
    let post_length = parse_to_html(text).len();
    // 4 for the space, two braces, and slash
    post_length + post_count_length + post_number_length + 4 <= character_limit
}

/// Split blog post into lists of posts that observe the character limit.
fn split_text(input: &str, character_limit: usize) -> Vec<String> {
    let input = input.replace('\r', "");

    let expected_post_count =
        ((parse_to_html(&input).len() / character_limit) as f64 * 1.5).ceil() as usize;
    debug!("Expect to create {} posts.", expected_post_count);

    let regex_text = "(?m:(?:\\.[ \t]+(?!\n))|(?:\n *\n))";
    let split_regex = SPLIT_POINT.get_or_init(|| Regex::new(&regex_text).unwrap());

    let split_points = split_regex
        .find_iter(&input)
        .map(|m| m.expect("error while splitting text").end())
        .collect::<Vec<_>>();

    let minimal_text_segments = split_at_indices(&input, &split_points).into_iter();
    let mut text_segments = Vec::new();
    let mut current_segment = String::new();
    for snippet_ref in minimal_text_segments {
        let expanded_segment = current_segment.clone() + snippet_ref;
        if is_under_post_limit(
            &expanded_segment,
            text_segments.len() + 1,
            expected_post_count,
            character_limit,
        ) {
            // We can add this text snippet to the current one.
            current_segment = expanded_segment;
        } else {
            // Segment has gotten too long, end it.
            text_segments.push(current_segment);
            current_segment = snippet_ref.to_string();
        }
    }
    if !current_segment.is_empty() {
        text_segments.push(current_segment);
    }

    let post_count = text_segments.len();
    text_segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| {
            parse_to_html(&format!("{} ({}/{})", segment, index + 1, post_count))
        })
        .collect()
}

/// Creates the client structure. We don't use any of elefren's app creation, registration, or OAuth authentication functionality,
/// instead this is a one-time (or repeated) manual process the user should execute.
fn create_client() -> Result<Mastodon, VarError> {
    Ok(Mastodon::from(elefren::Data {
        base: var("instance_url")?.into(),
        client_id: var("client_id")?.into(),
        client_secret: var("client_secret")?.into(),
        redirect: "https://github.com/kleinesfilmroellchen/fediblablag".into(),
        token: var("access_token")?.into(),
    }))
}

/// RAII guard that will delete all posts created when an error occurs later on.
struct PostDeleter<'c> {
    /// List of post IDs
    posts: Vec<String>,
    client: &'c Mastodon,
    is_armed: bool,
}

impl<'c> PostDeleter<'c> {
    pub fn new(client: &'c Mastodon) -> Self {
        Self {
            posts: Vec::new(),
            is_armed: true,
            client,
        }
    }

    pub fn add_post(&mut self, id: &str) {
        self.posts.push(id.into());
    }

    pub fn disarm(&mut self) {
        self.is_armed = false;
    }
}

impl<'c> Drop for PostDeleter<'c> {
    fn drop(&mut self) {
        if !self.is_armed {
            return;
        }
        debug!("Deleting all posts: {:?}", self.posts);
        for post in &self.posts {
            let result = self.client.delete_status(&post);
            match result {
                Err(why) => warn!("Couldn't delete post {}: {:?}", post, why),
                _ => {}
            }
        }
    }
}

fn post_series(
    client: &Mastodon,
    posts: &[String],
    options: &PostOptions,
) -> Result<(), elefren::Error> {
    let mut deleter = PostDeleter::new(client);

    let mut last_status = None;
    for post in posts {
        let mut status = StatusBuilder::new();
        status
            .status(post)
            .language(elefren::Language::Eng)
            .visibility(if last_status.is_none() {
                elefren::status_builder::Visibility::Public
            } else {
                elefren::status_builder::Visibility::Unlisted
            })
            .content_type("text/plain");
        if let Some(previous_status) = last_status {
            status.in_reply_to(previous_status);
        }
        if let Some(content_notice) = &options.content_notice {
            status.spoiler_text(content_notice);
        }
        let status = client.new_status(status.build()?)?;
        deleter.add_post(&status.id);
        last_status = Some(status.id);
        info!("Post created: {}", status.uri);
    }
    deleter.disarm();
    Ok(())
}

fn main() {
    dotenv::dotenv().ok();
    env_logger::init();

    let character_limit = var("character_limit")
        .expect("character limit environment variable not defined")
        .parse::<usize>()
        .expect("character limit environment variable is not an integer");

    let post_file = std::env::args()
        .nth(1)
        .expect("first argument must be a markdown file blog post to post");
    let post_md_text = std::fs::read_to_string(post_file).expect("couldn't read post file");
    let post_options = extract_options_from_frontmatter(&post_md_text);
    info!("Post options: {:#?}", post_options);
    let post_md_text = remove_frontmatter(&post_md_text);

    // Take CN into account by subtracting it from the actual character limit.
    let character_limit = character_limit
        - post_options
            .content_notice
            .as_ref()
            .map_or(0, |cn| cn.len());

    let text_sections = split_text(&post_md_text, character_limit);
    debug!(
        "Post lengths: {:?}",
        text_sections.iter().map(|t| t.len()).collect::<Vec<_>>()
    );

    if text_sections.iter().any(|t| t.len() > character_limit) {
        error!("At least one text section is over the character limit, aborting.");
        exit(1);
    }

    let client = create_client().expect("couldn't connect to instance");
    post_series(&client, &text_sections, &post_options).expect("posting failed");
}
