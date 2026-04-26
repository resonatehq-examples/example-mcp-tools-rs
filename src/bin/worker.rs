use resonate::prelude::*;

/// The durable workflow exposed to the MCP server.
///
/// This is a regular async function annotated with `#[resonate::function]` —
/// no decorators, no task queues, no special return types. The entire body is
/// retried automatically on failure, and intermediate steps wrapped in
/// `ctx.run` are checkpointed so a crash mid-flight resumes from the last
/// successful step.
#[resonate::function]
async fn get_forecast(ctx: &Context, latitude: f64, longitude: f64) -> Result<String> {
    // Step 1 — resolve the lat/lon to a forecast endpoint via the NWS points API.
    let points_url = format!("https://api.weather.gov/points/{latitude},{longitude}");
    let points: serde_json::Value = ctx.run(fetch_nws, points_url).await?;

    let forecast_url = points
        .get("properties")
        .and_then(|p| p.get("forecast"))
        .and_then(|f| f.as_str())
        .ok_or_else(|| Error::Application {
            message: "NWS response missing properties.forecast".into(),
        })?
        .to_string();

    // Step 2 — durable sleep. Survives crashes; resumes on the same wall clock.
    ctx.sleep(std::time::Duration::from_millis(500)).await?;

    // Step 3 — fetch the actual forecast.
    let forecast: serde_json::Value = ctx.run(fetch_nws, forecast_url).await?;

    // Step 4 — format the first five periods for the LLM.
    let periods = forecast
        .get("properties")
        .and_then(|p| p.get("periods"))
        .and_then(|p| p.as_array())
        .ok_or_else(|| Error::Application {
            message: "NWS response missing properties.periods".into(),
        })?;

    let formatted: Vec<String> = periods
        .iter()
        .take(5)
        .filter_map(|period: &serde_json::Value| {
            Some(format!(
                "{name}:\nTemperature: {temp}°{unit}\nWind: {wind} {dir}\nForecast: {detail}",
                name = period.get("name")?.as_str()?,
                temp = period.get("temperature")?.as_i64()?,
                unit = period.get("temperatureUnit")?.as_str()?,
                wind = period.get("windSpeed")?.as_str()?,
                dir = period.get("windDirection")?.as_str()?,
                detail = period.get("detailedForecast")?.as_str()?,
            ))
        })
        .collect();

    Ok(formatted.join("\n\n---\n\n"))
}

/// A leaf function — a single side effect Resonate retries on failure.
///
/// Because this is invoked via `ctx.run`, its return value is checkpointed to
/// the Resonate server: the workflow above can crash and resume without
/// re-issuing the HTTP request.
#[resonate::function]
async fn fetch_nws(url: String) -> Result<serde_json::Value> {
    let body = std::process::Command::new("curl")
        .arg("--silent")
        .arg("--fail")
        .arg("--max-time")
        .arg("10")
        .arg("-H")
        .arg("User-Agent: resonate-mcp-tools-rs/0.1 (example)")
        .arg("-H")
        .arg("Accept: application/geo+json")
        .arg(&url)
        .output()
        .map_err(|e| Error::Application {
            message: format!("curl spawn failed: {e}"),
        })?;

    if !body.status.success() {
        return Err(Error::Application {
            message: format!(
                "NWS request failed for {url}: exit {:?}",
                body.status.code()
            ),
        });
    }

    let parsed: serde_json::Value = serde_json::from_slice(&body.stdout)?;
    Ok(parsed)
}

#[tokio::main]
async fn main() {
    let resonate = Resonate::new(ResonateConfig {
        url: Some("http://localhost:8001".into()),
        group: Some("workers".into()),
        ..Default::default()
    });

    resonate.register(get_forecast).unwrap();
    resonate.register(fetch_nws).unwrap();

    println!("Worker started. Waiting for invocations on group `workers`...");
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");
}
