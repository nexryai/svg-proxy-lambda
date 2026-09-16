use axum::{
    body::Body,
    extract::Query,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use ipnet::IpNet;
use reqwest::{redirect::Policy, Url};
use resvg::{tiny_skia, usvg};
use serde::Deserialize;
use std::{
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    net::{lookup_host, TcpListener},
    sync::mpsc,
};
use tokio_stream::wrappers::ReceiverStream;

const DEFAULT_SIZE: u32 = 512;

const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 4096 * 4096;

const MAX_SVG_BYTES: usize = 5 * 1024 * 1024;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REDIRECTS: usize = 3;

const PNG_STREAM_CHUNK: usize = 64 * 1024;

static BLOCKED_NETS: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    [
        // IPv4
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",

        // IPv6
        "::/128",
        "::1/128",
        "::ffff:0:0/96",
        "64:ff9b::/96",
        "64:ff9b:1::/48",
        "100::/64",
        "2001::/23",
        "2001:db8::/32",
        "2002::/16",
        "fc00::/7",
        "fe80::/10",
        "ff00::/8",
    ]
    .into_iter()
    .map(|cidr| IpNet::from_str(cidr).expect("valid CIDR"))
    .collect()
});

static FONT_DB: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

#[derive(Debug, Deserialize)]
struct ConvertQuery {
    url: String,
    size: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Clone, Copy)]
struct RenderSize {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(convert))
        .route("/health", get(health));

    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind port 8080");

    axum::serve(listener, app)
        .await
        .expect("HTTP server failed");
}

async fn health() -> &'static str {
    "ok\n"
}

async fn convert(
    Query(query): Query<ConvertQuery>,
) -> Result<Response, AppError> {
    let render_size = parse_render_size(&query)?;

    let svg = fetch_svg(&query.url).await?;

    // resvg/usvg はCPU処理なのでasync runtimeを塞がないようにする。
    let pixmap = tokio::task::spawn_blocking(move || {
        render_svg(svg, render_size)
    })
    .await
    .map_err(|err| {
        AppError::internal(format!("render task failed: {err}"))
    })??;

    let body = png_stream_body(pixmap);

    let mut response = body.into_response();

    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/png"),
    );

    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );

    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );

    Ok(response)
}

fn parse_render_size(query: &ConvertQuery) -> Result<RenderSize, AppError> {
    let mut width = query.width;
    let mut height = query.height;

    if let Some(size) = query.size {
        if width.is_some() || height.is_some() {
            return Err(AppError::bad_request(
                "size cannot be combined with width or height",
            ));
        }

        if size == 0 {
            return Err(AppError::bad_request(
                "size must be greater than zero",
            ));
        }

        width = Some(size);
        height = Some(size);
    }

    for value in [width, height].into_iter().flatten() {
        if value == 0 {
            return Err(AppError::bad_request(
                "dimensions must be greater than zero",
            ));
        }

        if value > MAX_DIMENSION {
            return Err(AppError::bad_request(format!(
                "dimensions must not exceed {MAX_DIMENSION}"
            )));
        }
    }

    Ok(RenderSize { width, height })
}

fn render_svg(
    svg: Vec<u8>,
    requested: RenderSize,
) -> Result<tiny_skia::Pixmap, AppError> {
    let mut options = usvg::Options::default();

    /*
     * 重要:
     *
     * usvg のデフォルト image resolver はローカルファイルを
     * <image href="..."> から読み込める。
     *
     * 公開SVGプロキシでこれは不要かつ危険なので完全に無効化。
     * data:image/... は通常通り許可する。
     */
    options.image_href_resolver.resolve_string =
        Box::new(|_, _| None);

    options.fontdb = Arc::clone(&FONT_DB);

    let tree = usvg::Tree::from_data(&svg, &options)
        .map_err(|err| {
            AppError::unprocessable(format!(
                "invalid or unsupported SVG: {err}"
            ))
        })?;

    let source_size = tree.size();

    let (width, height) = resolve_dimensions(
        requested,
        source_size.width(),
        source_size.height(),
    )?;

    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| {
            AppError::internal("failed to allocate render buffer")
        })?;

    let scale_x = width as f32 / source_size.width();
    let scale_y = height as f32 / source_size.height();

    let transform =
        tiny_skia::Transform::from_scale(scale_x, scale_y);

    resvg::render(
        &tree,
        transform,
        &mut pixmap.as_mut(),
    );

    Ok(pixmap)
}

fn resolve_dimensions(
    requested: RenderSize,
    source_width: f32,
    source_height: f32,
) -> Result<(u32, u32), AppError> {
    let aspect = source_width as f64 / source_height as f64;

    let (width, height) = match (requested.width, requested.height) {
        (None, None) => {
            let width = DEFAULT_SIZE;
            let height =
                ((width as f64 / aspect).round() as u32).max(1);

            (width, height)
        }

        (Some(width), None) => {
            let height =
                ((width as f64 / aspect).round() as u32).max(1);

            (width, height)
        }

        (None, Some(height)) => {
            let width =
                ((height as f64 * aspect).round() as u32).max(1);

            (width, height)
        }

        (Some(width), Some(height)) => (width, height),
    };

    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(AppError::bad_request(format!(
            "dimensions must not exceed {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }

    let pixels = width as u64 * height as u64;

    if pixels > MAX_PIXELS {
        return Err(AppError::bad_request(
            "image contains too many pixels",
        ));
    }

    Ok((width, height))
}

async fn fetch_svg(raw_url: &str) -> Result<Vec<u8>, AppError> {
    tokio::time::timeout(
        FETCH_TIMEOUT,
        fetch_svg_inner(raw_url),
    )
    .await
    .map_err(|_| AppError::bad_gateway("upstream request timed out"))?
}

async fn fetch_svg_inner(raw_url: &str) -> Result<Vec<u8>, AppError> {
    let mut url = Url::parse(raw_url)
        .map_err(|_| AppError::bad_request("invalid URL"))?;

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_url(&url)?;

        let (host, ip) = resolve_public_address(&url).await?;

        /*
         * DNS rebinding防止:
         *
         * 自前で解決・検査したIPへreqwestを固定する。
         * TLSのSNI/Hostは元のhostnameのまま。
         */
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .resolve(
                &host,
                SocketAddr::new(ip, 443),
            )
            .build()
            .map_err(|err| {
                AppError::internal(format!(
                    "failed to create HTTP client: {err}"
                ))
            })?;

        let response = client
            .get(url.clone())
            .header(
                reqwest::header::ACCEPT,
                "image/svg+xml,application/xml;q=0.9,text/xml;q=0.8",
            )
            .header(
                reqwest::header::USER_AGENT,
                "svg-proxy-lambda/2.0",
            )
            .send()
            .await
            .map_err(|err| {
                AppError::bad_gateway(format!(
                    "failed to fetch SVG: {err}"
                ))
            })?;

        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(AppError::bad_gateway(
                    "too many redirects",
                ));
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| {
                    AppError::bad_gateway(
                        "redirect without Location header",
                    )
                })?
                .to_str()
                .map_err(|_| {
                    AppError::bad_gateway(
                        "invalid redirect Location header",
                    )
                })?;

            // 相対redirectにも対応
            url = url.join(location).map_err(|_| {
                AppError::bad_gateway(
                    "invalid redirect target",
                )
            })?;

            // 次のループでredirect先も再度SSRF検証
            continue;
        }

        if !response.status().is_success() {
            return Err(AppError::bad_gateway(format!(
                "upstream returned HTTP {}",
                response.status().as_u16()
            )));
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_SVG_BYTES as u64)
        {
            return Err(AppError::bad_gateway(
                "SVG exceeds size limit",
            ));
        }

        let mut data = Vec::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| {
                AppError::bad_gateway(format!(
                    "failed to read upstream body: {err}"
                ))
            })?;

            if data.len() + chunk.len() > MAX_SVG_BYTES {
                return Err(AppError::bad_gateway(
                    "SVG exceeds size limit",
                ));
            }

            data.extend_from_slice(&chunk);
        }

        return Ok(data);
    }

    Err(AppError::bad_gateway("too many redirects"))
}

fn validate_url(url: &Url) -> Result<(), AppError> {
    if url.scheme() != "https" {
        return Err(AppError::bad_request(
            "only HTTPS URLs are allowed",
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::bad_request(
            "URLs containing credentials are not allowed",
        ));
    }

    if let Some(port) = url.port() {
        if port != 443 {
            return Err(AppError::bad_request(
                "only HTTPS port 443 is allowed",
            ));
        }
    }

    let host = url.host_str().ok_or_else(|| {
        AppError::bad_request("URL has no hostname")
    })?;

    // "localhost" や検索ドメイン経由などを拒否
    if host.parse::<IpAddr>().is_err() && !host.contains('.') {
        return Err(AppError::bad_request(
            "hostname must be fully qualified",
        ));
    }

    Ok(())
}

async fn resolve_public_address(
    url: &Url,
) -> Result<(String, IpAddr), AppError> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::bad_request("URL has no hostname"))?
        .to_owned();

    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err(AppError::bad_request(
                "private or special-purpose IP address is not allowed",
            ));
        }

        return Ok((host, ip));
    }

    let resolved: Vec<IpAddr> = lookup_host((host.as_str(), 443))
        .await
        .map_err(|_| {
            AppError::bad_gateway("failed to resolve hostname")
        })?
        .map(|addr| addr.ip())
        .collect();

    if resolved.is_empty() {
        return Err(AppError::bad_gateway(
            "hostname resolved to no addresses",
        ));
    }

    /*
     * 一つでもprivate/bogonが混ざっていたらhost全体を拒否する。
     * public + private のDNS応答を利用したSSRFも防ぐ。
     */
    if resolved.iter().copied().any(|ip| !is_public_ip(ip)) {
        return Err(AppError::bad_request(
            "hostname resolves to a private or special-purpose address",
        ));
    }

    // Lambda環境で扱いやすいIPv4を優先
    let selected = resolved
        .iter()
        .copied()
        .find(|ip| ip.is_ipv4())
        .unwrap_or(resolved[0]);

    Ok((host, selected))
}

fn is_public_ip(ip: IpAddr) -> bool {
    !BLOCKED_NETS
        .iter()
        .any(|network| network.contains(&ip))
}

/*
 * PNGを全体Vec<u8>へencodeせず、
 * encoderから生成されたデータをbody streamへ渡す。
 */
fn png_stream_body(pixmap: tiny_skia::Pixmap) -> Body {
    let width = pixmap.width();
    let height = pixmap.height();

    /*
     * tiny-skia内部はpremultiplied RGBA。
     * PNGへ保存するときは通常RGBAへ戻す必要がある。
     */
    let rgba = pixmap.take_demultiplied();

    let (tx, rx) =
        mpsc::channel::<Result<Bytes, io::Error>>(8);

    tokio::task::spawn_blocking(move || {
        let mut output = ChannelWriter::new(tx);

        let result = (|| -> Result<(), png::EncodingError> {
            let mut encoder =
                png::Encoder::new(&mut output, width, height);

            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);

            let mut writer = encoder.write_header()?;

            writer.write_image_data(&rgba)?;
            writer.finish()?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                let _ = output.flush();
            }

            Err(err) => {
                output.send_error(io::Error::other(
                    err.to_string(),
                ));
            }
        }
    });

    Body::from_stream(ReceiverStream::new(rx))
}

struct ChannelWriter {
    tx: mpsc::Sender<Result<Bytes, io::Error>>,
    buffer: Vec<u8>,
}

impl ChannelWriter {
    fn new(
        tx: mpsc::Sender<Result<Bytes, io::Error>>,
    ) -> Self {
        Self {
            tx,
            buffer: Vec::with_capacity(PNG_STREAM_CHUNK),
        }
    }

    fn send_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let data = std::mem::replace(
            &mut self.buffer,
            Vec::with_capacity(PNG_STREAM_CHUNK),
        );

        self.tx
            .blocking_send(Ok(Bytes::from(data)))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "response stream closed",
                )
            })
    }

    fn send_error(&mut self, err: io::Error) {
        let _ = self.tx.blocking_send(Err(err));
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);

        if self.buffer.len() >= PNG_STREAM_CHUNK {
            self.send_buffer()?;
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_buffer()
    }
}
