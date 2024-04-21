use std::ops::ControlFlow;

type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() {
    let mut args = parse_args();

    take_arg!(target from args);
    take_arg!(dist   from args);

    let path = format!("./{dist}");
    let target = target.replacen("{}", &dist, 1);

    for idx in 1.. {
        let path = format!("{path}/{idx:02}");
        let target = target.replacen("{}", &format!("{idx:02}"), 1);

        tokio::fs::create_dir_all(&target).await.unwrap();

        for jdx in 1.. {
            let path = format!("{path}/{jdx:04}");
            let target = target.replacen("{}", &format!("{jdx:04}"), 1);

            let ptimg = match try_use_cache_otherwise_fetch(
                &format!("{path}.ptimg.json"),
                &target.replacen("{}", "ptimg.json", 1),
            )
            .await
            {
                Ok(ControlFlow::Continue(b)) => b,
                Ok(ControlFlow::Break(e)) | Err(e) => {
                    eprintln!("error reported: {e}");
                    break;
                }
            };

            let rdimg = match try_use_cache_otherwise_fetch(
                &format!("{path}.jpg"),
                &target.replacen("{}", "jpg", 1),
            )
            .await
            {
                Ok(ControlFlow::Continue(b)) => b,
                Ok(ControlFlow::Break(e)) | Err(e) => {
                    eprintln!("error reported: {e}");
                    break;
                }
            };

            let ogimg = {
                let src = image::load_from_memory(&rdimg).unwrap();

                let pt = serde_json::from_slice::<Ptimg>(&ptimg).unwrap();
                pt.restore(|_| &src).remove(0)
            };

            ogimg
                .write_to(
                    &mut tokio::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(format!("{path}.webp"))
                        .await
                        .unwrap()
                        .try_into_std()
                        .unwrap(),
                    image::ImageFormat::WebP,
                )
                .unwrap();
        }
    }
}

fn parse_args() -> std::collections::HashMap<String, String> {
    let mut args = std::collections::HashMap::new();

    let None = std::env::args().fold(None, |state, arg| match state {
        None => {
            if let Some(ident) = arg.strip_prefix("--") {
                Some(ident.to_owned())
            } else {
                eprintln!("unrecognized arguments: {arg}");
                None
            }
        }

        Some(ident) => {
            if let Some(old) = args.insert(ident, arg) {
                eprintln!("ignored arguments: {old}");
                None
            } else {
                None
            }
        }
    }) else {
        eprintln!("unterminated arguments");
        std::process::exit(1)
    };

    args
}

#[macro_export]
macro_rules! take_arg {
    ($key:ident from $args:expr) => {
        let Some($key) = $args.remove(stringify!($key)) else {
            eprintln!("couldn't recognize {}", stringify!($key));
            std::process::exit(1)
        };
    };
}

async fn try_use_cache_otherwise_fetch(
    path: &str,
    target: &str,
) -> Result<ControlFlow<Error, Vec<u8>>> {
    match tokio::fs::OpenOptions::new().read(true).open(path).await {
        Ok(mut f) => {
            use tokio::io::AsyncReadExt;

            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes).await?;

            Ok(ControlFlow::Continue(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use tokio::io::AsyncWriteExt;

            let res = match reqwest::get(target).await?.error_for_status() {
                Ok(r) => r,
                Err(e) => return Ok(ControlFlow::Break(e.into())),
            };

            let bytes = res.bytes().await?.to_vec();

            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await?
                .write_all(&bytes)
                .await?;

            Ok(ControlFlow::Continue(bytes))
        }
        Err(e) => Err(e)?,
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(unused)]
struct Ptimg {
    ptimg_version: usize,
    resources: std::collections::HashMap<String, Resource>,
    views: Vec<View>,
}

impl Ptimg {
    fn restore<'a>(&self, map: impl Fn(&str) -> &'a image::DynamicImage) -> Vec<image::RgbaImage> {
        self.views
            .iter()
            .map(|v| {
                let mut dst = image::RgbaImage::new(v.width, v.height);

                v.coords
                    .iter()
                    .map(parse)
                    .for_each(|(key, rep)| rep.apply(map(key), &mut dst));

                dst
            })
            .collect::<Vec<_>>()
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(unused)]
struct Resource {
    src: String,
    width: usize,
    height: usize,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(unused)]
struct View {
    width: u32,
    height: u32,
    coords: Vec<String>,
}

struct Vec2<T> {
    x: T,
    y: T,
}

impl<T> Vec2<T> {
    fn new(x: T, y: T) -> Self {
        Self { x, y }
    }
}

struct Replacer {
    size: Vec2<u32>,
    src: Vec2<u32>,
    dst: Vec2<i64>,
}

impl Replacer {
    fn new(size: Vec2<u32>, src: Vec2<u32>, dst: Vec2<i64>) -> Self {
        Self { size, src, dst }
    }
}

impl Replacer {
    fn apply<T, U>(&self, src: &T, dst: &mut U)
    where
        T: image::GenericImageView<Pixel = U::Pixel>,
        U: image::GenericImage,
    {
        use image::imageops::{crop_imm, replace};

        let part = crop_imm(src, self.src.x, self.src.y, self.size.x, self.size.y);
        replace(dst, &*part, self.dst.x, self.dst.y);
    }
}

fn parse(s: &impl AsRef<str>) -> (&str, Replacer) {
    use nom::bytes::complete::tag;
    use nom::character::complete::{alpha1, digit1};
    use nom::combinator::{all_consuming, map, map_res};
    use nom::sequence::separated_pair;
    use nom::IResult;
    use std::str::FromStr;

    fn num<T: FromStr>(s: &str) -> IResult<&str, T> {
        map_res(digit1, |s: &str| s.parse::<T>())(s)
    }

    fn vec<T: FromStr>(s: &str) -> IResult<&str, Vec2<T>> {
        map(separated_pair(num, tag(","), num), |(l, r)| Vec2::new(l, r))(s)
    }

    let src = separated_pair(vec, tag("+"), vec);
    let bdy = separated_pair(src, tag(">"), vec);
    let whl = separated_pair(alpha1, tag(":"), bdy);

    match all_consuming(whl)(s.as_ref()) {
        Ok(("", (key, ((src, size), dst)))) => (key, Replacer::new(size, src, dst)),

        Err(e) => panic!("{e}"),
        _ => unreachable!(),
    }
}
