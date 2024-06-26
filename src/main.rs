use std::{borrow::Cow, collections::HashMap, io::Write, net::SocketAddr, sync::Arc};

use axum::{response::IntoResponse, Router};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

/** レートリミット対象の処理が終わった時に破棄する*/
struct RateLimitTracker(Option<RateLimit>,Option<String>,tokio::runtime::Handle);
impl Drop for RateLimitTracker{
	fn drop(&mut self) {
		let s=self.1.take().unwrap();
		let hosts=self.0.take().unwrap().hosts;
		self.2.spawn(async move{
			tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
			let mut wlock=hosts.write().await;
			let active_tasks=wlock.remove(&s).unwrap_or(0);
			if active_tasks>1{
				wlock.insert(s,active_tasks-1);
			}
		});
	}
}
#[derive(Clone,Debug)]
struct RateLimit{
	hosts:Arc<tokio::sync::RwLock<HashMap<String,u32>>>,
}
impl RateLimit{
	/** 処理を実行しても良いか確認し、ロックを取得する。Err(true)は待てば成功する可能性がある*/
	async fn request(&self,url:&str)->Result<RateLimitTracker,bool>{
		let host=reqwest::Url::parse(url).map_err(|_|false)?;
		let host=host.host().ok_or(false)?.to_string();
		let rlock=self.hosts.read().await;
		let active_tasks=rlock.get(&host).copied().unwrap_or(0);
		drop(rlock);
		let active_tasks=active_tasks+1;
		if active_tasks<3{
			//処理中がこれ含め3件未満であれば即時実行
			let mut wlock=self.hosts.write().await;
			wlock.insert(host.clone(),active_tasks);
			let handle=tokio::runtime::Handle::current();
			Ok(RateLimitTracker(Some(self.clone()),Some(host),handle))
		}else{
			//再試行するべき失敗
			Err(true)
		}
	}
}
#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ConfigFile{
	bind_addr: String,
	timeout:u64,
	user_agent:String,
	max_size:u32,
	proxy:Option<String>,
	media_proxy:Option<String>,
	append_headers:Vec<String>,
}
impl ConfigFile{
	fn append_headers(&self,headers:&mut axum::http::HeaderMap){
		use std::str::FromStr;
		for line in self.append_headers.iter(){
			if let Some(idx)=line.find(":"){
				if idx+1>=line.len(){
					continue;
				}
				if let Ok(k)=axum::http::HeaderName::from_str(&line[0..idx]){
					if let Ok(v)=line[idx+1..].parse(){
						headers.append(k,v);
					}
				}
			}
		}
	}
}
#[derive(Debug, Deserialize)]
pub struct RequestParams{
	url: String,
	lang:Option<String>,
	#[serde(rename = "userAgent")]
	user_agent:Option<String>,
	#[serde(rename = "responseTimeout")]
	response_timeout:Option<u32>,
	#[serde(rename = "contentLengthLimit")]
	content_length_limit:Option<u32>,
}
#[derive(Debug,Serialize,Deserialize)]
pub struct SummalyPlayer{
	url:Option<String>,
	width:Option<f64>,
	height:Option<f64>,
	allow:Vec<String>,
}
#[derive(Debug,Serialize,Deserialize)]
pub struct SummalyResult{
	url:String,
	title:Option<String>,
	icon:Option<String>,
	description:Option<String>,
	thumbnail:Option<String>,
	sitename:Option<String>,
	player:serde_json::Value,
	sensitive:bool,
	#[serde(rename = "activityPub")]
	activity_pub:Option<String>,
	oembed:Option<OEmbed>,
}
#[derive(Debug,Serialize,Deserialize)]
pub struct OEmbed{
	r#type:String,
	version:String,
	title:Option<String>,
	author_name:Option<String>,
	author_url:Option<String>,
	provider_name:Option<String>,
	provider_url:Option<String>,
	cache_age:Option<f64>,
	thumbnail_url:Option<String>,
	thumbnail_width:Option<f64>,
	thumbnail_height:Option<f64>,
	url:Option<String>,//type=photo
	html:Option<String>,//type=video/rich
	width:Option<f64>,
	height:Option<f64>,
}
async fn shutdown_signal() {
	use tokio::signal;
	use futures::{future::FutureExt,pin_mut};
	let ctrl_c = async {
		signal::ctrl_c()
			.await
			.expect("failed to install Ctrl+C handler");
	}.fuse();

	#[cfg(unix)]
	let terminate = async {
		signal::unix::signal(signal::unix::SignalKind::terminate())
			.expect("failed to install signal handler")
			.recv()
			.await;
	}.fuse();
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>().fuse();
	pin_mut!(ctrl_c, terminate);
	futures::select!{
		_ = ctrl_c => {},
		_ = terminate => {},
	}
}
fn main() {
	let config_path=match std::env::var("SUMMALY_CONFIG_PATH"){
		Ok(path)=>{
			if path.is_empty(){
				"config.json".to_owned()
			}else{
				path
			}
		},
		Err(_)=>"config.json".to_owned()
	};
	if !std::path::Path::new(&config_path).exists(){
		let default_config=ConfigFile{
			bind_addr: "0.0.0.0:12267".to_owned(),
			timeout:5000,
			user_agent: "https://github.com/yojo-art/summaly-rs".to_owned(),
			max_size:2*1024*1024,
			proxy:None,
			media_proxy:None,//e.g. https://misskey.example.com/proxy/
			append_headers:[
				"Content-Security-Policy:default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".to_owned(),
				"Access-Control-Allow-Origin:*".to_owned(),
			].to_vec(),
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path).expect("create default config.json").write_all(default_config.as_bytes()).unwrap();
	}
	let config:ConfigFile=serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();
	let config=Arc::new(config);
	let client=reqwest::ClientBuilder::new();
	let client=client.build().unwrap();
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let limit=RateLimit{
		hosts:Arc::new(tokio::sync::RwLock::new(HashMap::new()))
	};
	let arg_tup=(client,config,limit);
	rt.block_on(async{
		let http_addr:SocketAddr = arg_tup.1.bind_addr.parse().unwrap();
		let app = Router::new();
		let arg_tup0=arg_tup.clone();
		let app=app.route("/",axum::routing::get(move|headers,parms|get_file(None,headers,arg_tup0.clone(),parms)));
		let app=app.route("/*path",axum::routing::get(move|path,headers,parms|get_file(Some(path),headers,arg_tup.clone(),parms)));
		let comression_layer= tower_http::compression::CompressionLayer::new()
		.gzip(true);
		let app=app.layer(comression_layer);
		let listener = tokio::net::TcpListener::bind(&http_addr).await.unwrap();
		axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await.unwrap();
	});
}
async fn get_file(
	_path:Option<axum::extract::Path<String>>,
	request_headers:axum::http::HeaderMap,
	(client,config,limit):(reqwest::Client,Arc<ConfigFile>,RateLimit),
	axum::extract::Query(q):axum::extract::Query<RequestParams>,
)->axum::response::Response{
	println!("{}\t{}\tlang:{:?}\tresponse_timeout:{:?}\tcontent_length_limit:{:?}\tuser_agent:{:?}",
		chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
		q.url,
		q.lang,
		q.user_agent,
		q.response_timeout,
		q.content_length_limit,
	);
	if q.url.starts_with("coffee://"){
		let mut headers=axum::http::HeaderMap::new();
		headers.append("X-Proxy-Error","I'm a teapot".parse().unwrap());
		config.append_headers(&mut headers);
		return (axum::http::StatusCode::IM_A_TEAPOT,headers).into_response()
	}
	for _ in 0..3{
		match limit.request(&q.url).await{
			Ok(_)=>{
				return remote_request(request_headers,(client,config),q).await;
			},
			Err(true)=>{
				tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
			},
			_=>{
				let mut headers=axum::http::HeaderMap::new();
				headers.append(axum::http::header::CACHE_CONTROL,"public, max-age=30".parse().unwrap());
				config.append_headers(&mut headers);
				return (axum::http::StatusCode::BAD_REQUEST,headers).into_response();
			}
		}
	}
	let mut headers=axum::http::HeaderMap::new();
	headers.append(axum::http::header::CACHE_CONTROL,"public, max-age=30".parse().unwrap());
	config.append_headers(&mut headers);
	(axum::http::StatusCode::TOO_MANY_REQUESTS,headers).into_response()
}
async fn remote_request(
		request_headers:axum::http::HeaderMap,
		(client,config):(reqwest::Client,Arc<ConfigFile>),
		q:RequestParams,
	)->axum::response::Response{
	let builder=client.get(&q.url);
	let user_agent=q.user_agent.as_ref().unwrap_or_else(||&config.user_agent);
	let builder=builder.header(reqwest::header::USER_AGENT,user_agent);
	let builder=if let Some(lang)=q.lang{
		builder.header(reqwest::header::ACCEPT_LANGUAGE,lang)
	}else{
		builder
	};
	let timeout_ms=config.timeout.min(q.response_timeout.unwrap_or(u32::MAX) as u64);
	let builder=builder.timeout(std::time::Duration::from_millis(timeout_ms));
	let content_length_limit=q.content_length_limit.unwrap_or(config.max_size);
	let resp=builder.send().await;
	let resp=match resp{
		Ok(resp)=>resp,
		Err(e)=>{
			let mut headers=axum::http::HeaderMap::new();
			headers.append("X-Proxy-Error",e.to_string().parse().unwrap());
			config.append_headers(&mut headers);
			return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,headers).into_response()
		},
	};
	let v=match load_all(resp,content_length_limit.into()).await{
		Ok(v)=>v,
		Err(e)=>{
			let mut headers=axum::http::HeaderMap::new();
			headers.append("X-Proxy-Error",e.parse().unwrap());
			config.append_headers(&mut headers);
			return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,headers).into_response()
		},
	};
	let mut meta_charset=None;
	let mut content_type=None;
	{
		let mut doc=String::new();
		let meta_charset_b="<meta ".as_bytes();
		let uppercase_meta_charset_b="<META ".as_bytes();
		let mut i=0;
		let mut bb=vec![];
		for ch in v.iter(){
			if meta_charset_b.len()>i{
				if *ch==meta_charset_b[i]||*ch==uppercase_meta_charset_b[i]{
					i+=1;
				}else{
					i=0;
				}
			}else if *ch==b'>'{
				i=0;
				if let Ok(c)=std::str::from_utf8(&bb){
					doc+="<meta ";
					doc+=c;
					doc+=">\n"
				}
				bb.clear();
			}else{
				bb.push(*ch);
			}
		}
		if let Ok(doc)=html_parser::Dom::parse(&doc){
			for node in doc.children.iter(){
				if let Some(e)=node.element(){
					let http_equiv=e.attributes.get("http-equiv").unwrap_or(&None).as_ref().map(|s|s.to_lowercase());
					match (
						http_equiv.as_ref().map(|v|v.as_str()),
						e.attributes.get("content").unwrap_or(&None).as_ref(),
					){
						(Some("content-type"),Some(content))=>{
							content_type=Some(content.to_owned());
						},
						_ => {},
					}
					if let Some(s)=e.attributes.get("charset").unwrap_or(&None){
						meta_charset=Some(s.to_owned());
					}
				}
			}
		}
	}
	let mut encoding=None;
	if let Some(content_type)=&content_type{
		//content_type="text/html;charset=shift_jis"
		for c in content_type.split(';'){
			let c=c.to_lowercase();
			if let Some(i)=c.find("charset="){
				let charset=&c[i+"charset=".len()..];
				encoding=encoding_rs::Encoding::for_label(charset.as_bytes());
			}
		}
	}
	if let Some(meta_charset)=&meta_charset{
		if let Some(e)=encoding_rs::Encoding::for_label(meta_charset.as_bytes()){
			encoding=Some(e);
		}
	}
	if encoding==Some(encoding_rs::UTF_8){
		encoding=None;
	}
	let mut dst=Cow::Borrowed("");
	if let Some(encoding)=encoding{
		(dst,_,_)=encoding.decode(&v);
	}
	let s=if dst.is_empty(){
		String::from_utf8_lossy(&v)
	}else{
		dst
	};
	let start=match s.find("<head").or_else(||s.find("<HEAD")){
		Some(idx)=>idx,
		None=>{
			let mut headers=axum::http::HeaderMap::new();
			headers.append("X-Proxy-Error","no head".parse().unwrap());
			config.append_headers(&mut headers);
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
		},
	};
	let end=match s.find("</head>").or_else(||s.find("</HEAD>")){
		Some(idx)=>idx,
		None=>{
			let mut headers=axum::http::HeaderMap::new();
			headers.append("X-Proxy-Error","no /head".parse().unwrap());
			config.append_headers(&mut headers);
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
		},
	};
	let s=&s[start+6..end];
	let dom=match html_parser::Dom::parse(s){
		Ok(idx)=>idx,
		Err(e)=>{
			let mut headers=axum::http::HeaderMap::new();
			headers.append("X-Proxy-Error",e.to_string().parse().unwrap());
			config.append_headers(&mut headers);
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
		},
	};
	let base_url=if let Ok(url)=reqwest::Url::parse(&q.url){
		url
	}else{
		reqwest::Url::parse("https://localhost").unwrap()
	};
	let base_url_str=format!("{}://{}{}",base_url.scheme(),base_url.host_str().unwrap(),base_url.port().map(|n|format!(":{n}")).unwrap_or_default());
	let mut player=SummalyPlayer{
		url: None,
		width: None,
		height: None,
		allow: vec![],
	};
	let mut resp=SummalyResult{
		title: None,
		icon: None,
		description: None,
		thumbnail: None,
		sitename: None,
		player: serde_json::json!({}),
		sensitive: false,
		activity_pub: None,
		url: q.url.clone(),
		oembed:None,
	};
	for node in dom.children.iter(){
		if let html_parser::Node::Element(element)=node{
			if element.name.as_str()=="title"{
				if resp.title.is_none(){//og:title優先
					let mut s=String::new();
					for e in element.children.iter(){
						if let Some(c)=e.text(){
							s+=c;
						}
					}
					let ts=s.trim();
					if !ts.is_empty(){
						if ts==s.as_str(){
							resp.title=Some(s);
						}else{
							resp.title=Some(ts.to_owned());
						}
					}
				}
			}
			match (element.name.as_str(),&element.attributes){
				("meta",att)=>{
					match att.get("name").unwrap_or(&None).as_ref().map(|s|(
						s.as_str(),
						att.get("content").unwrap_or(&None).as_ref().map(|s|html_escape::decode_html_entities(s.trim())),
					)){
						Some(("msapplication-tooltip",Some(content))) => {
							if resp.description.is_none(){//og:description優先
								resp.description=Some(content.into());
							}
						},
						Some(("application-name",Some(content))) => {
							if resp.sitename.is_none(){//og:site_name優先
								resp.sitename=Some(content.to_string());
							}
							if resp.title.is_none(){//og:title優先
								resp.title=Some(content.to_string());
							}
						},
						_=>{}
					}
					match att.get("property").unwrap_or(&None).as_ref().map(|s|(
						s.as_str(),
						att.get("content").unwrap_or(&None).as_ref().map(|s|html_escape::decode_html_entities(s.trim())),
					)){
						Some(("og:image",Some(content))) => {
							resp.thumbnail=Some(content.into());
						},
						Some(("og:url",Some(content))) => {
							resp.url=content.into();
						},
						Some(("og:title",Some(content))) => {
							resp.title=Some(content.into());
						},
						Some(("og:description",Some(content))) => {
							resp.description=Some(content.into());
						},
						Some(("description",Some(content))) => {
							resp.description=Some(content.into());
						},
						Some(("og:site_name",Some(content))) => {
							resp.sitename=Some(content.into());
						},
						Some(("og:video:url",Some(content))) => {
							if player.url.is_none(){//og:video:secure_url優先
								player.url=Some(content.into());
							}
						},
						Some(("og:video:secure_url",Some(content))) => {
							player.url=Some(content.into());
						},
						Some(("og:video:width",Some(content))) => {
							if let Ok(content)=content.parse::<f64>(){
								player.width=Some(content);
							}
						},
						Some(("og:video:height",Some(content))) => {
							if let Ok(content)=content.parse::<f64>(){
								player.height=Some(content);
							}
						},
						_ => {},
					}
				},
				("link",att)=>{
					match att.get("rel").unwrap_or(&None).as_ref().map(|s|(
						s.as_str(),
						att.get("href").unwrap_or(&None).as_ref().map(|s|html_escape::decode_html_entities(s.trim())),
						att.get("type").unwrap_or(&None).as_ref().map(|t|t.as_str()),
					)){
						Some(("shortcut icon",Some(href),_)) => {
							if resp.icon.is_none(){//icon優先
								resp.icon=Some(href.into());
							}
						},
						Some(("icon",Some(href),_)) => {
							resp.icon=Some(href.into());
						},
						Some(("apple-touch-icon",Some(href),_)) => {
							if resp.thumbnail.is_none(){//og:image優先
								resp.thumbnail=Some(href.into());
							}
						},
						Some(("alternate",Some(href),Some("application/json+oembed"))) => {
							let embed_res=if let Ok(mut href)=urlencoding::decode(&href){
								if let Some(s)=solve_url(&href,&base_url,&base_url_str,&None,""){
									href=Cow::Owned(s);
								}
								let builder=client.get(href.as_ref());
								let user_agent=q.user_agent.as_ref().unwrap_or_else(||&config.user_agent);
								let builder=builder.header(reqwest::header::USER_AGENT,user_agent);
								let timeout_ms=config.timeout.min(q.response_timeout.unwrap_or(u32::MAX) as u64);
								let builder=builder.timeout(std::time::Duration::from_millis(timeout_ms));
								builder.send().await.map_err(|e|{
									println!("oembed {} {:?}",href,e);
								}).ok()
							}else{
								None
							};
							let embed_json=if let Some(embed_res)=embed_res{
								if let Ok(d)=load_all(embed_res,content_length_limit.into()).await{
									serde_json::from_slice(&d).ok()
								}else{
									None
								}
							}else{
								None
							};
							if let Some(v)=embed_json{
								resp.oembed=Some(v);
							}
						},
						_ => {},
					}
				},
				_=>{}
			}
		}
	}
	if let Some(v)=&resp.oembed{
		if let Some(width)=v.width{
			player.width=Some(width);
		}
		if let Some(height)=v.height{
			player.height=Some(height);
		}
		const SAFE_LIST:[&'static str;6] = [
			"autoplay",
			"clipboard-write",
			"fullscreen",
			"encrypted-media",
			"picture-in-picture",
			"web-share",
		];
		if let Some(html)=v.html.as_ref().map(|v|v.as_str()){
			if let Ok(html)=html_parser::Dom::parse(html){
				for node in html.children.iter(){
					if let html_parser::Node::Element(node)=node{
						if let Some(Some(allow))=node.attributes.get("allow"){
							for allow in allow.split(";"){
								let allow=allow.trim();
								if SAFE_LIST.contains(&allow){
									player.allow.push(allow.to_owned());
								}
							}
						}
					}
				}
			}
		}
	}
	//すべての有効なプレイヤーにはurlが存在する
	if player.url.is_some(){
		if let Ok(player)=serde_json::to_value(player){
			resp.player=player;
		}
	}
	if resp.icon.is_none(){
		resp.icon=Some(format!("{}/favicon.ico",base_url_str));
	}
	if let Some(Some(icon))=resp.icon.as_ref().map(|s|
		solve_url(s,&base_url,&base_url_str,&config.media_proxy,"icon.webp")
	){
		resp.icon=Some(icon);
	}
	if let Some(Some(thumbnail))=resp.thumbnail.as_ref().map(|s|
		solve_url(s,&base_url,&base_url_str,&config.media_proxy,"thumbnail.webp")
	){
		resp.thumbnail=Some(thumbnail);
	}
	if let Some(url)=solve_url(&resp.url,&base_url,&base_url_str,&None,""){
		resp.url=url;
	}
	if let Ok(json)=serde_json::to_string(&resp){
		let mut headers=axum::http::HeaderMap::new();
		headers.append(axum::http::header::CONTENT_TYPE,"application/json".parse().unwrap());
		headers.append(axum::http::header::CACHE_CONTROL,"public, max-age=1800".parse().unwrap());
		config.append_headers(&mut headers);
		(axum::http::StatusCode::OK,headers,json).into_response()
	}else{
		axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
	}
}
async fn load_all(resp: reqwest::Response,content_length_limit:u64)->Result<Vec<u8>,String>{
	let len_hint=resp.content_length().unwrap_or(content_length_limit);
	if len_hint>content_length_limit{
		return Err(format!("lengthHint:{}>{}",len_hint,content_length_limit));
	}
	let mut response_bytes=Vec::with_capacity(len_hint as usize);
	let mut stream=resp.bytes_stream();
	while let Some(x) = stream.next().await{
		match x{
			Ok(b)=>{
				if response_bytes.len()+b.len()>content_length_limit as usize{
					return Err(format!("length:{}>{}",response_bytes.len()+b.len(),content_length_limit))
				}
				response_bytes.extend_from_slice(&b);
			},
			Err(e)=>{
				return Err(format!("LoadAll:{:?}",e))
			}
		}
	}
	Ok(response_bytes)
}
fn solve_url(icon:&str,base_url:&reqwest::Url,base_url_str:&str,media_proxy:&Option<String>,proxy_filename:&str)->Option<String>{
	let icon=if icon.starts_with("//"){
		Cow::Owned(format!("{}:{}",base_url.scheme(),icon))
	}else if icon.starts_with("/"){
		Cow::Owned(format!("{}{}",base_url_str,icon))
	}else if !icon.starts_with("http"){
		let buf=std::path::PathBuf::from(base_url.path());
		let buf=buf.join(std::path::Path::new(icon));
		if let Some(s)=buf.to_str(){
			let mut path_list=vec![];
			for part in s.split("/"){
				if part.is_empty()||part=="."{

				}else if part==".."{
					if path_list.len()>0{
						path_list.remove(path_list.len()-1);
					}
				}else{
					path_list.push(part);
				}
			}
			let mut path_string=base_url_str.to_owned();
			for part in path_list{
				path_string+="/";
				path_string+=part;
			}
			Cow::Owned(path_string)
		}else{
			return None;
		}
	}else{
		Cow::Borrowed(icon)
	};
	if let Some(media_proxy)=&media_proxy{
		Some(format!("{}{}?url={}",media_proxy,proxy_filename,urlencoding::encode(&icon)))
	}else{
		Some(icon.into_owned())
	}
}
