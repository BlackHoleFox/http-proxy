mod error;

use error::{
    ChunkingRequest, ChunkingResponse, InvalidPath, MakingResponseBody, RequestError, RequestIssue,
};
use http::request::Parts;
use hyper::{
    body::Body,
    server::{conn::AddrStream, Server},
    service, Request, Response,
};
use snafu::ResultExt;
use std::{
    convert::TryFrom,
    env,
    error::Error,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use tracing::{debug, error, info};
use tracing_log::LogTracer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use twilight_http::{client::Client, request::Request as TwilightRequest, routing::Path};
use std::time::Instant;
use std::sync::Arc;

#[cfg(feature = "expose-metrics")]
use metrics::timing;
#[cfg(feature = "expose-metrics")]
use metrics_runtime::{observers::PrometheusBuilder, Receiver};
#[cfg(feature = "expose-metrics")]
use metrics_core::{Builder, Drain};


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    LogTracer::init()?;

    let log_filter_layer =
        EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?;
    let log_fmt_layer = fmt::layer();

    let log_subscriber = tracing_subscriber::registry()
        .with(log_filter_layer)
        .with(log_fmt_layer);

    tracing::subscriber::set_global_default(log_subscriber)?;

    let host_raw = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let host = IpAddr::from_str(&host_raw)?;
    let port = env::var("PORT").unwrap_or_else(|_| "80".into()).parse()?;

    let client = Client::new(env::var("DISCORD_TOKEN")?);

    let address = SocketAddr::from((host, port));

    #[cfg(feature = "metrics")]
        {
            let receiver = Receiver::builder()
                .build()
                .expect("Failed to create receiver!");

            receiver.install();
            let builder = Arc::new(PrometheusBuilder::new());
        }

    // The closure inside `make_service_fn` is run for each connection,
    // creating a 'service' to handle requests for that specific connection.
    let service = service::make_service_fn(move |addr: &AddrStream| {
        debug!("Connection from: {:?}", addr);
        let client = client.clone();
        #[cfg(feature = "metrics")]
            let builder = builder.clone();
        async move {
            Ok::<_, RequestError>(service::service_fn(move |incoming: Request<Body>| {
                #[cfg(feature = "expose-metrics")]
                    {
                        handle_request(client.clone(), incoming, Some(builder.clone()))
                    }
                #[cfg(not(feature = "expose-metrics"))]
                    {
                        handle_request(client.clone(), incoming, None)
                    }
            }))
        }
    });


    let server = Server::bind(&address).serve(service);

    info!("Listening on http://{}", address);

    if let Err(why) = server.await {
        error!("Fatal server error: {}", why);
    }

    Ok(())
}

fn path_name(path: &Path) -> &'static str {
    match path {
        Path::ChannelsId(..) => "Channel",
        Path::ChannelsIdInvites(..) => "Channel invite",
        Path::ChannelsIdMessages(..) => "Channel message",
        Path::ChannelsIdMessagesBulkDelete(..) => "Bulk delete message",
        Path::ChannelsIdMessagesId(..) => "Channel message",
        Path::ChannelsIdMessagesIdReactions(..) => "Message reaction",
        Path::ChannelsIdMessagesIdReactionsUserIdType(..) => "Message reaction for user",
        Path::ChannelsIdPermissionsOverwriteId(..) => "Channel permission override",
        Path::ChannelsIdPins(..) => "Channel pins",
        Path::ChannelsIdPinsMessageId(..) => "Specific channel pin",
        Path::ChannelsIdTyping(..) => "Typing indicator",
        Path::ChannelsIdWebhooks(..) => "Webhook",
        Path::Gateway => "Gateway",
        Path::GatewayBot => "Gateway bot info",
        Path::Guilds => "Guilds",
        Path::GuildsId(..) => "Guild",
        Path::GuildsIdBans(..) => "Guild bans",
        Path::GuildsIdAuditLogs(..) => "Guild audit logs",
        Path::GuildsIdBansUserId(..) => "Guild ban for user",
        Path::GuildsIdChannels(..) => "Guild channel",
        Path::GuildsIdWidget(..) => "Guild widget",
        Path::GuildsIdEmojis(..) => "Guild emoji",
        Path::GuildsIdEmojisId(..) => "Specific guild emoji",
        Path::GuildsIdIntegrations(..) => "Guild integrations",
        Path::GuildsIdIntegrationsId(..) => "Specific guild integration",
        Path::GuildsIdIntegrationsIdSync(..) => "Sync guild integration",
        Path::GuildsIdInvites(..) => "Guild invites",
        Path::GuildsIdMembers(..) => "Guild members",
        Path::GuildsIdMembersId(..) => "Specific guild member",
        Path::GuildsIdMembersIdRolesId(..) => "Guild member role",
        Path::GuildsIdMembersMeNick(..) => "Modify own nickname",
        Path::GuildsIdPreview(..) => "Guild preview",
        Path::GuildsIdPrune(..) => "Guild prune",
        Path::GuildsIdRegions(..) => "Guild region",
        Path::GuildsIdRoles(..) => "Guild roles",
        Path::GuildsIdRolesId(..) => "Specific guild role",
        Path::GuildsIdVanityUrl(..) => "Guild vanity invite",
        Path::GuildsIdWebhooks(..) => "Guild webhooks",
        Path::InvitesCode => "Invite info",
        Path::UsersId => "User info",
        Path::UsersIdConnections => "User connections",
        Path::UsersIdChannels => "User channels",
        Path::UsersIdGuilds => "User in guild",
        Path::UsersIdGuildsId => "Guild from user",
        Path::VoiceRegions => "Voice region list",
        Path::WebhooksId(..) => "Webhook",
        Path::OauthApplicationsMe => "Current application info",
        _ => "Unknown path!"
    }
}

async fn handle_request<T>(
    client: Client,
    request: Request<Body>,
    builder: Option<Arc<T>>,
) -> Result<Response<Body>, RequestError> {
    debug!("Incoming request: {:?}", request);

    let (parts, body) = request.into_parts();
    let Parts {
        method,
        uri,
        headers,
        ..
    } = parts;

    let trimmed_path = if uri.path().starts_with("/api/v6") {
        uri.path().replace("/api/v6", "")
    } else {
        uri.path().to_owned()
    };

    #[cfg(feature = "expose-metrics")]
    if trimmed_path == "/metrics" {
        // this is only none when the feature flag is off, this could would not exist then
        return Ok(Response::builder().body(Body::from(builder.unwrap().build().drain())).unwrap());
    }

    let path = match Path::try_from((method.clone(), trimmed_path.as_ref())).context(InvalidPath) {
        Ok(path) => path,
        Err(e) => {
            error!("Error determining path for {}: {:?}", trimmed_path, e);
            return Err(e);
        }
    };

    let bytes = (hyper::body::to_bytes(body).await.context(ChunkingRequest)?).to_vec();

    let path_and_query = match uri.path_and_query() {
        Some(v) => v.as_str().replace("/api/v6/", "").into(),
        None => {
            debug!("No path in URI: {:?}", uri);

            return Err(RequestError::NoPath { uri });
        }
    };
    let body = if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    };

    let p = path_name(&path);
    let m = method.to_string();
    let raw_request = TwilightRequest {
        body,
        form: None,
        headers: Some(headers),
        method,
        path,
        path_str: path_and_query,
    };

    let start = Instant::now();
    let resp = client.raw(raw_request).await.context(RequestIssue)?;

    let status = resp.status();
    let resp_headers = resp.headers().clone();

    let bytes = resp.bytes().await.context(ChunkingResponse)?;
    let end = Instant::now();

    let mut builder = Response::builder().status(status);

    if let Some(headers) = builder.headers_mut() {
        headers.extend(resp_headers);
    }

    let resp = builder
        .body(Body::from(bytes))
        .context(MakingResponseBody)?;

    debug!("Response: {:?}", resp);

    #[cfg(feature = "expose-metrics")]
    timing!("gearbot_proxy_requests", start, end, "method"=>m.to_string(), "route"=>p, "status"=>resp.status().to_string());
    info!("{} {}: {}", m, p, resp.status());

    Ok(resp)
}
