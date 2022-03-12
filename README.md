# Nanoleaf AWS Terminator 🤖

![CI](https://github.com/abrefort/rs-nanoleaf-aws-terminator/actions/workflows/rust.yml/badge.svg)

## What is this thing ?

> ⚠️ **This tool was written for fun, but using it _will_ lead to data loss :** use at your own risk on a test account and triple check your current AWS SDK configuration.

Nanoleaf AWS Terminator enables you terminate random resources using your Nanoleaf Shapes modular light panels.
The tool currently supports the following resources :

* EC2 Instances (yellow)
* DynamoDB Tables  (blue)
* S3 Buckets (green)

When Nanoleaf AWS Terminator is running your panels will light up according to discovered resources. A single touch on a panel will terminate/delete the underlying resource and turn off the panel.

Resources are continualy discovered.

## Use cases

While it may be tempting to discard this tool as useless, here are some use cases :

* 🔨 Playing whack-a-mole against your Auto Scaling groups (check CloudWatch for your score) 
* 🐵 Introducing kids to Chaos Engineering
* 💸 Lowering your bill (up to **100%** costs reduction)

## Demo

Youtube link :
[![demo](https://img.youtube.com/vi/ifYDYwsTbYk/0.jpg)](https://www.youtube.com/watch?v=ifYDYwsTbYk)


## Usage

Install Rust with https://rustup.rs/.

You will need the IP of your Nanoleaf controller and an API token generated in the following way :

```bash
# Replace x.x.x.x with the IP of your Nanoleaf controller 

# First : hold the power button on your controller until the LEDs start flashing.

# Then you have 30 seconds to :
curl --location --request POST 'http://x.x.x.x:16021/api/v1/new'
```

Then :

```bash
# Set AWS_PROFILE to something safe
export AWS_PROFILE=my_test_account_profile

cargo run -- -h
```

## Architecture

The state is stored in a bidirectionnal map (panel <=> resource) and a vector (available panels) which are shared (using two `Arc<Mutex<T>>`) between the following tokio tasks :

* `process_events` : receives events from the Nanoleaf controller and terminates resources.
* `manage_display` : updates panel lighting
* `monitor_ec2` : discovers EC2 instances
* `monitor_s3` : discovers S3 buckets
* `monitor_ec2` : discovers EC2 instances

## Issues

* One big `main.rs` : should be split.
* No error handling at all : `unwrap()` all the things !
* Bad code quality, certainly bad tokio usage : this project was a good excuse to read the tokio documentation .
* There is a semaphore that should protect against the risk of reading a resource in `monitor_*` while `process_events` is deleting it. Unfortunately it means that parrallelism between `monitor_*` tasks is lost : it should be split, or rewritten using another approach.