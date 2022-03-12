#![allow(clippy::too_many_arguments)]
  
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_ec2::model::Filter as EC2Filter;
use aws_types::region::Region;
use bimap::BiHashMap;
use bimap::BiMap;
use clap::Parser;
use colored::*;
use env_logger::Env;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_repr::Deserialize_repr;
use std::fmt::Write;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use tokio::time;
use tokio::time::Interval;
use tokio::sync::Semaphore;

/// Simple program to terminate random AWS resources whack-a-mole style, a Nanoleaf Shapes controller is required.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// IPv4 of the Nanoleaf controller
    #[clap(short, long, env)]
    api_ip: Ipv4Addr,

    /// token for the Nanoleaf controller
    #[clap(short = 't', long, env)]
    api_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Panels {
    name: String,
    panel_layout: PanelLayout,
}

#[derive(Debug, Deserialize)]
struct PanelLayout {
    layout: Layout,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Layout {
    num_panels: u16,
    position_data: PositionData,
}

type PositionData = Vec<Panel>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Panel {
    panel_id: u16,
    shape_type: ShapeType,
}

#[derive(Deserialize_repr, PartialEq, Debug)]
#[repr(u8)]
enum ShapeType {
    Triangle = 0,
    Rhythm = 1,
    Square = 2,
    ControlSquareMaster = 3,
    ControlSquarePassive = 4,
    HexagonShapes = 7,
    TriangleShapes = 8,
    MiniTriangleShapes = 9,
    ShapesController = 12,
    ElementsHexagons = 14,
    ElementsHexagonsCorner = 15,
    LinesConnector = 16,
    LightLines = 17,
    LightLinesSingleZone = 18,
    ControllerCap = 19,
    PowerConnector = 20,
}

#[derive(Debug, Deserialize)]
struct EventData {
    events: Vec<TouchEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TouchEvent {
    panel_id: u16,
}

#[derive(Debug, Serialize)]
enum NanoleafCommand {
    #[serde(rename = "write")]
    #[serde(rename_all = "camelCase")]
    Custom {
        command: String,
        anim_type: String,
        anim_data: String,
        #[serde(rename = "loop")]
        looping: bool,
        palette: Vec<String>, //todo
    },
}

#[derive(Clone, Debug, Hash, PartialEq, std::cmp::Eq)]
enum AWSResource {
    Instance(String),
    Table(String),
    Bucket(String),
}

type SharedPanels = Arc<Mutex<Vec<u16>>>;
type SharedDisplay = Arc<Mutex<BiHashMap<u16, AWSResource>>>;

fn task_log(log_level: &str, task_name: &str, message: &str) {
    match log_level {
        "info" => info!(
            "{}{} {}{} {}",
            "[".blue(),
            "task".bold(),
            task_name.magenta(),
            "]".blue(),
            message
        ),
        "warn" => warn!(
            "{}{} {}{} {}",
            "[".blue(),
            "task".bold(),
            task_name.magenta(),
            "]".blue(),
            message
        ),
        "error" => error!(
            "{}{} {}{} {}",
            "[".blue(),
            "task".bold(),
            task_name.magenta(),
            "]".blue(),
            message
        ),
        _ => (),
    }
}

fn get_resource_from_panel(
    display: Arc<Mutex<BiMap<u16, AWSResource>>>,
    panel_id: u16,
) -> AWSResource {
    let display = display.lock().unwrap();
    let resource = display.get_by_left(&panel_id).unwrap().clone();
    resource
}

async fn get_panels(client: &reqwest::Client, api_url: &str) -> Panels {
    let resp = client
        .get(format!("{}/", api_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let panels: Panels = serde_json::from_str(&resp).unwrap();
    panels
}

async fn send_nanoleaf_display(client: &reqwest::Client, api_url: &str, data: &str) {
    let command = NanoleafCommand::Custom {
        command: "display".to_string(),
        anim_type: "static".to_string(),
        anim_data: data.into(),
        looping: false,
        palette: vec![],
    };

    let _res = client
        .put(format!("{}/effects", api_url))
        .body(serde_json::to_string(&command).unwrap())
        .send()
        .await
        .unwrap();
}

async fn initialize_panels(
    client: &reqwest::Client,
    api_url: &str,
    panel_count: &u16,
    panels: &Panels,
    available_panels: &SharedPanels,
) {
    info!("Initializing panels");
    // initialize animation data
    let mut data = String::new();
    write!(data, "{}", panel_count).unwrap();

    // loop over panels
    for panel in &panels.panel_layout.layout.position_data {
        if panel.shape_type != ShapeType::ShapesController {
            // turn panel off over 20x0.1s
            write!(data, " {} 1 0 0 0 0 20", panel.panel_id).unwrap();

            // add to available panels
            let mut available_panels = available_panels.lock().unwrap();
            available_panels.push(panel.panel_id);
        } else {
            continue;
        }
    }

    // send command to nanoleaf controller
    send_nanoleaf_display(client, api_url, &data).await;
    info!("Panels initialized");
}

async fn process_events(
    semaphore: &Arc<Semaphore>,
    client: &reqwest::Client,
    api_url: &str,
    ec2_client: &aws_sdk_ec2::Client,
    s3_client: &aws_sdk_s3::Client,
    dynamodb_client: &aws_sdk_dynamodb::Client,
    available_panels: &SharedPanels,
    display: &SharedDisplay,
) {
    let task_name = "process_events";
    // get an event stream
    task_log("info", task_name, "Getting event stream from controller");
    let mut stream = client
        .get(format!("{}/events", api_url))
        .query(&[("id", "4")])
        .send()
        .await
        .unwrap()
        .bytes_stream()
        .eventsource();

    // process events
    while let Some(event) = stream.next().await {
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let data: EventData = serde_json::from_str(&event.unwrap().data).unwrap();

        for touch_event in data.events {
            let panel_id = touch_event.panel_id;

            let display_clone = display.clone();
            let resource = get_resource_from_panel(display_clone, panel_id);

            task_log(
                "info",
                task_name,
                format!(
                    "Received touch event for panel {}",
                    panel_id.to_string().green()
                )
                .as_str(),
            );

            // resource type specific termination
            match resource {
                AWSResource::Instance(id) => {
                    let req = ec2_client.terminate_instances().instance_ids(&id);
                    let _resp = req.send().await.unwrap();
                    task_log(
                        "info",
                        task_name,
                        format!("Terminated {} {}", "EC2 Instance".yellow(), id.bold()).as_str(),
                    );

                    // wait a second for the instance to leave the Running status
                    time::sleep(time::Duration::from_secs(1)).await;
                }
                AWSResource::Bucket(name) => {
                    let req = s3_client.delete_bucket().bucket(&name);
                    let _resp = req.send().await.unwrap();
                    task_log(
                        "info",
                        task_name,
                        format!("Terminated {} {}", "S3 Bucket".green(), name.bold()).as_str(),
                    );

                    // wait a second for the bucket to be destroyed
                    time::sleep(time::Duration::from_secs(1)).await;
                }
                AWSResource::Table(name) => {
                    let req = dynamodb_client.delete_table().table_name(&name);
                    let _resp = req.send().await.unwrap();
                    task_log(
                        "info",
                        task_name,
                        format!("Terminated {} {}", "DynamoDB Table".blue(), name.bold()).as_str(),
                    );

                    // wait a second for the table to leave
                    time::sleep(time::Duration::from_secs(1)).await;
                }
            }

            // free up panel and make it available
            let mut display = display.lock().unwrap();
            display.remove_by_left(&panel_id);

            let mut available_panels = available_panels.lock().unwrap();
            available_panels.push(panel_id);
            task_log(
                "info",
                task_name,
                format!("Panel {} is now available", panel_id.to_string().green()).as_str(),
            );
        }
        drop(permit);
    }
}

async fn monitor_ec2(
    semaphore: &Arc<Semaphore>,
    client: &aws_sdk_ec2::Client,
    available_panels: &SharedPanels,
    display: &SharedDisplay,
    mut interval: tokio::time::Interval,
) {
    let task_name = "monitor_ec2";
    loop {
        // wait at least interval between two loops
        interval.tick().await;

        // take permit to run
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        let mut instance_ids = Vec::new();

        // get running instances
        let req = client.describe_instances().filters(
            EC2Filter::builder()
                .name("instance-state-name")
                .values("running")
                .build(),
        );
        let resp = req.send().await.unwrap();

        // check if there are any reservations
        let reservations = match resp.reservations {
            Some(reservations) => {
                if !reservations.is_empty() {
                    reservations
                } else {
                    continue;
                }
            }
            None => continue,
        };

        // loop over reservation and instances
        for reservation in reservations {
            let instances = match reservation.instances {
                Some(instances) => instances,
                None => continue,
            };

            for instance in instances {
                instance_ids.push(instance.instance_id.unwrap())
            }
        }

        for id in instance_ids {
            let mut display = display.lock().unwrap();

            // check if instance is already displayed
            if display.contains_right(&AWSResource::Instance(id.to_string())) {
                continue;
            } else {
                task_log(
                    "info",
                    task_name,
                    format!("Found new {} {}", "EC2 Instance".yellow(), id.bold()).as_str(),
                );
                let mut available_panels = available_panels.lock().unwrap();
                let available_panels_count = available_panels.len();

                // insert instance in display
                display.insert(
                    available_panels.swap_remove(fastrand::usize(..available_panels_count)),
                    AWSResource::Instance(id.to_string()),
                );
            }
        }

        drop(permit);
    }
}

async fn monitor_s3(
    semaphore: &Arc<Semaphore>,
    client: &aws_sdk_s3::Client,
    region: &Region,
    available_panels: &SharedPanels,
    display: &SharedDisplay,
    mut interval: tokio::time::Interval,
) {
    let task_name = "monitor_s3";
    loop {
        // wait at least interval between two loops
        interval.tick().await;

        // take permit to run
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        // get buckets
        let req = client.list_buckets();
        let resp = req.send().await.unwrap();

        // check if there are any buckets
        let buckets = match resp.buckets {
            Some(buckets) => {
                if !buckets.is_empty() {
                    buckets
                } else {
                    continue;
                }
            }
            None => continue,
        };

        // loop over buckets
        for bucket in buckets {
            let name = bucket.name.unwrap();

            // check bucket region
            // TODO : support for us-east-1
            let req = client.get_bucket_location().bucket(&name);
            let resp = req.send().await.unwrap();

            if resp.location_constraint.unwrap().as_str() != region.to_string() {
                continue;
            }

            let mut display = display.lock().unwrap();

            // check if bucket is already displayed
            if display.contains_right(&AWSResource::Bucket(name.clone())) {
                continue;
            } else {
                task_log(
                    "info",
                    task_name,
                    format!("Found new {} {}", "S3 Bucket".green(), name.bold()).as_str(),
                );

                let mut available_panels = available_panels.lock().unwrap();
                let available_panels_count = available_panels.len();

                // insert bucket in display
                display.insert(
                    available_panels.swap_remove(fastrand::usize(..available_panels_count)),
                    AWSResource::Bucket(name),
                );
            }
        }

        drop(permit);
    }
}

async fn monitor_dynamodb(
    semaphore: &Arc<Semaphore>,
    client: &aws_sdk_dynamodb::Client,
    available_panels: &SharedPanels,
    display: &SharedDisplay,
    mut interval: tokio::time::Interval,
) {
    let task_name = "monitor_dynamodb";
    loop {
        // wait at least interval between two loops
        interval.tick().await;

        // take permit to run
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        // get tables
        let req = client.list_tables();
        let resp = req.send().await.unwrap();

        // check if there are any tables
        let table_names = match resp.table_names {
            Some(table_names) => {
                if !table_names.is_empty() {
                    table_names
                } else {
                    continue;
                }
            }
            None => continue,
        };

        // loop over tables
        for name in table_names {
            // check tables status
            let req = client.describe_table().table_name(&name);
            let resp = req.send().await.unwrap();

            if resp.table.unwrap().table_status.unwrap()
                != aws_sdk_dynamodb::model::TableStatus::Active
            {
                continue;
            }

            let mut display = display.lock().unwrap();

            // check if table is already displayed
            if display.contains_right(&AWSResource::Table(name.clone())) {
                continue;
            } else {
                task_log(
                    "info",
                    task_name,
                    format!("Found new {} {}", "DynamoDB Table".blue(), name.bold()).as_str(),
                );

                let mut available_panels = available_panels.lock().unwrap();
                let available_panels_count = available_panels.len();

                // insert table in display
                display.insert(
                    available_panels.swap_remove(fastrand::usize(..available_panels_count)),
                    AWSResource::Table(name),
                );
            }
        }

        drop(permit);
    }
}

async fn manage_display(
    client: &reqwest::Client,
    api_url: &str,
    panel_count: &u16,
    available_panels: &SharedPanels,
    display: &SharedDisplay,
    mut interval: Interval,
) {
    loop {
        // wait at least interval between two loops
        interval.tick().await;

        let mut data = String::new();
        write!(data, "{}", panel_count).unwrap();

        // set color according to service
        for (panel, resource) in display.lock().unwrap().iter() {
            let color = match *resource {
                AWSResource::Instance(_) => "247 142 4 0",
                AWSResource::Table(_) => "77 114 243 0",
                AWSResource::Bucket(_) => "96 163 55 0",
            };

            write!(data, " {} 1 {} 20", panel, color).unwrap();
        }

        // set remaining panels colors
        for panel in available_panels.lock().unwrap().iter() {
            write!(data, " {} 1 0 0 0 0 20", panel).unwrap();
        }

        // send command to nanoleaf controller
        send_nanoleaf_display(client, api_url, &data).await;
    }
}

#[tokio::main]
async fn main() {
    // logging
    log_panics::init();
    env_logger::Builder::from_env(
        Env::default().default_filter_or("error,rs_nanoleaf_aws_terminator=info"),
    )
    .init();

    // params
    let args = Args::parse();

    let ip = args.api_ip;
    let token = args.api_token;

    let api_url = format!("http://{}:16021/api/v1/{}", ip, token);

    // Semaphore 
    let semaphore = Arc::new(Semaphore::new(1));

    // display and panels states
    let available_panels: SharedPanels = Arc::new(Mutex::new(vec![]));
    let display: SharedDisplay = Arc::new(Mutex::new(BiMap::<u16, AWSResource>::new()));

    // AWS configuration
    let aws_shared_config = aws_config::load_from_env().await;
    let aws_region = RegionProviderChain::default_provider()
        .region()
        .await
        .unwrap();
    // client for ec2
    let ec2_client = aws_sdk_ec2::Client::new(&aws_shared_config);
    // client for s3
    let s3_client = aws_sdk_s3::Client::new(&aws_shared_config);
    // client for dynamodb
    let dynamodb_client = aws_sdk_dynamodb::Client::new(&aws_shared_config);

    // client for nanoleaf connections
    let client = reqwest::Client::new();

    // get panels
    info!("Getting panels from controller at {}", ip);
    let panels = get_panels(&client, &api_url).await;
    let panel_count = panels.panel_layout.layout.num_panels - 1; // using Shapes with a control panel
    info!("Got {} panels from {} controller", panel_count, panels.name);

    // initialize panels and display
    initialize_panels(&client, &api_url, &panel_count, &panels, &available_panels).await;

    // declare task to monitor nanoleaf touch events
    let semaphore_clone = semaphore.clone();
    let client_clone = client.clone();
    let api_url_clone = api_url.clone();
    let ec2_client_clone = ec2_client.clone();
    let s3_client_clone = s3_client.clone();
    let dynamodb_client_clone = dynamodb_client.clone();
    let display_clone = display.clone();
    let available_panels_clone = available_panels.clone();

    let process_events_task = async move {
        process_events(
            &semaphore_clone,
            &client_clone,
            &api_url_clone,
            &ec2_client_clone,
            &s3_client_clone,
            &dynamodb_client_clone,
            &available_panels_clone,
            &display_clone,
        )
        .await
    };

    // declare task to monitor ec2 instances
    let semaphore_clone = semaphore.clone();
    let ec2_client_clone = ec2_client.clone();
    let display_clone = display.clone();
    let available_panels_clone = available_panels.clone();
    let ec2_interval = time::interval(time::Duration::from_secs(1));

    let monitor_ec2_task = async move {
        monitor_ec2(
            &semaphore_clone,
            &ec2_client_clone,
            &available_panels_clone,
            &display_clone,
            ec2_interval,
        )
        .await;
    };

    // declare task to monitor s3 buckets
    let semaphore_clone = semaphore.clone();
    let s3_client_clone = s3_client.clone();
    let display_clone = display.clone();
    let available_panels_clone = available_panels.clone();
    let s3_interval = time::interval(time::Duration::from_secs(1));

    let monitor_s3_task = async move {
        monitor_s3(
            &semaphore_clone,
            &s3_client_clone,
            &aws_region,
            &available_panels_clone,
            &display_clone,
            s3_interval,
        )
        .await;
    };

    // declare task to monitor dynamodb tables
    let semaphore_clone = semaphore.clone();
    let dynamodb_client_clone = dynamodb_client.clone();
    let display_clone = display.clone();
    let available_panels_clone = available_panels.clone();
    let dynamodb_interval = time::interval(time::Duration::from_secs(1));

    let monitor_dynamodb_task = async move {
        monitor_dynamodb(
            &semaphore_clone,
            &dynamodb_client_clone,
            &available_panels_clone,
            &display_clone,
            dynamodb_interval,
        )
        .await;
    };

    // declare task to manage display
    let client_clone = client.clone();
    let api_url_clone = api_url.clone();
    let available_panels_clone = available_panels.clone();
    let display_clone = display.clone();
    let display_interval = time::interval(time::Duration::from_secs(1));

    let manage_display_task = async move {
        manage_display(
            &client_clone,
            &api_url_clone,
            &panel_count,
            &available_panels_clone,
            &display_clone,
            display_interval,
        )
        .await
    };

    // spawn tasks
    info!("Spawning tasks");
    tokio::select! {
        result = tokio::spawn(process_events_task) => {
            match result {
                Ok(_) => (),
                Err(_) => {
                    task_log(
                        "error",
                        "process_event",
                        "task panicked, aborting !");
                    std::process::exit(1);
                }
            }
        },
        result = tokio::spawn(monitor_ec2_task) => {
            match result {
                Ok(_) => (),
                Err(_) => {
                    task_log(
                        "error",
                        "monitor_ec2",
                        "task panicked, aborting !");
                    std::process::exit(1);
                }
            }
        },
        result = tokio::spawn(monitor_s3_task) => {
            match result {
                Ok(_) => (),
                Err(_) => {
                    task_log(
                        "error",
                        "monitor_s3",
                        "task panicked, aborting !");
                    std::process::exit(1);
                }
            }
        },
        result = tokio::spawn(monitor_dynamodb_task) => {
            match result {
                Ok(_) => (),
                Err(_) => {
                    task_log(
                        "error",
                        "monitor_dynamodb",
                        "task panicked, aborting !");
                    std::process::exit(1);
                }
            }
        },
        result = tokio::spawn(manage_display_task) => {
            match result {
                Ok(_) => (),
                Err(_) => {
                    task_log(
                        "error",
                        "manage_display",
                        "task panicked, aborting !");
                    std::process::exit(1);
                }
            }
        }
    }
}
