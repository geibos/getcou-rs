use anyhow::{anyhow, Context, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use reqwest::Client;
use std::{
    env::args,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::Duration,
};
use tempfile::tempdir_in;
use tokio;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = args().collect();
    if args.len() != 3 {
        print_help();
        return Err(anyhow!("Invalid number of arguments"));
    }

    let url = &args[1];
    let output_file = Path::new(&args[2]);
    touch(output_file)?;

    let temp_dir = tempdir_in(".")?;
    println!("Using temporary directory: {}", temp_dir.path().display());

    // Download main playlist
    let main_playlist = download_with_retry(url, 3).await.context("Failed to download main playlist")?;
    
    // Determine secondary playlist
    let secondary_content = if contains_direct_segments(&main_playlist) {
        main_playlist
    } else {
        let last_line = main_playlist
            .lines()
            .rev()
            .find(|line| line.starts_with("http"))
            .ok_or_else(|| anyhow!("No valid playlist URL found in main playlist"))?;
        download_with_retry(last_line, 3).await.context("Failed to download secondary playlist")?
    };

    // Download segments
    let segment_urls: Vec<&str> = secondary_content
        .lines()
        .filter(|line| line.starts_with("http"))
        .collect();

    println!("Found {} video segments", segment_urls.len());
    if segment_urls.is_empty() {
        return Err(anyhow!("No video segments found in playlist"));
    }

    // Download segments concurrently (10 at a time)
    let client = Client::new();
    let mut futures = FuturesUnordered::new();
    let mut completed_segments = 0;
    let total_segments = segment_urls.len();

    for (i, url) in segment_urls.iter().enumerate() {
        let segment_path = temp_dir.path().join(format!("{:05}.ts", i));
        let client_clone = client.clone();
        let url = url.to_string();
        
        futures.push(async move {
            download_segment(&client_clone, &url, &segment_path, 12).await
        });

        // Process completed futures and maintain concurrency limit
        while futures.len() >= 10 {
            if let Some(result) = futures.next().await {
                match result {
                    Ok(_) => {
                        completed_segments += 1;
                        println!("Downloaded segment {}/{}", completed_segments, total_segments);
                    }
                    Err(e) => {
                        eprintln!("Failed to download segment: {}", e);
                        return Err(e);
                    }
                }
            }
        }
    }

    // Wait for remaining futures
    while let Some(result) = futures.next().await {
        match result {
            Ok(_) => {
                completed_segments += 1;
                println!("Downloaded segment {}/{}", completed_segments, total_segments);
            }
            Err(e) => {
                eprintln!("Failed to download segment: {}", e);
                return Err(e);
            }
        }
    }

    // Concatenate segments
    concatenate_files(temp_dir.path(), output_file)?;

    println!(
        "Download completed successfully. Output file:\n{}",
        output_file.display()
    );
    Ok(())
}

fn contains_direct_segments(content: &str) -> bool {
    content.lines().any(|line| {
        line.starts_with("http") && 
        (line.contains(".ts") || line.contains(".bin"))
    })
}

async fn download_with_retry(url: &str, max_retries: usize) -> Result<String> {
    let client = Client::new();
    let mut last_error = None;

    for attempt in 0..=max_retries {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                return resp.text().await.context("Failed to read response body")
            }
            Ok(resp) => last_error = Some(anyhow!("HTTP status: {}", resp.status())),
            Err(e) => last_error = Some(e.into()),
        }

        if attempt < max_retries {
            let delay = 2u64.pow(attempt as u32);
            eprintln!("Retry {}/{} in {}s...", attempt + 1, max_retries, delay);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("Unknown error")))
}

async fn download_segment(client: &Client, url: &str, path: &Path, max_retries: usize) -> Result<()> {
    let mut last_error = None;

    for attempt in 0..=max_retries {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.context("Failed to read response bytes")?;
                tokio::fs::write(path, bytes).await.context("Failed to write file")?;
                return Ok(());
            }
            Ok(resp) => last_error = Some(anyhow!("HTTP status: {}", resp.status())),
            Err(e) => last_error = Some(e.into()),
        }

        if attempt < max_retries {
            let delay = 2u64.pow(attempt as u32);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("Failed after {} retries", max_retries)))
}

fn concatenate_files(temp_dir: &Path, output_path: &Path) -> Result<()> {
    let mut output_file = File::create(output_path)?;
    let mut entries: Vec<PathBuf> = fs::read_dir(temp_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "ts"))
        .collect();

    entries.sort();

    for entry in entries {
        let mut segment_file = File::open(&entry)?;
        io::copy(&mut segment_file, &mut output_file)?;
    }

    Ok(())
}

fn touch(path: &Path) -> Result<()> {
    File::create(path)?;
    Ok(())
}

fn print_help() {
    println!(
        r#"
The first argument should be a playlist link found in the page source of GetCourse.
Example: <video id="vgc-player_html5_api" data-master="your_link_here" ... />.
The second argument should be the output file path (recommended extension: .ts).
Example: "How to download videos from GetCourse.ts"

Copy the link and run the script like:
$ getcourse-downloader "playlist_url" "output_file.ts"

Graphical instructions: https://github.com/mikhailnov/getcourse-video-downloader
Report issues: https://github.com/mikhailnov/getcourse-video-downloader/issues
"#
    );
}