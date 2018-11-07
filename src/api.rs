use actix_web::{dev, error, multipart, http};
use actix_web::{AsyncResponder, FutureResponse, HttpMessage, HttpRequest, HttpResponse, Json, Path, Result, State,};
use actix_web::error::{ErrorBadRequest,};

use futures::prelude::*;
use futures::future;
use std::cell::RefCell;
use std::clone::Clone;
use std::fs;
use std::io::Write;
use std::io;
use std::path;
use std::rc::Rc;
use std::sync::Arc;
use tempfile::NamedTempFile;
use chrono::{Utc};
use jwt;

use app::{AppState,Claims};
use errors::ApiError;
use db::{CreateBuild, CreateBuildRef, LookupBuild, LookupBuildRef, StartCommitJob, StartPublishJob};
use models::{NewBuildRef};
use actix_web::ResponseError;
use tokens::{self, ClaimsValidator};

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenSubsetArgs {
    sub: String,
    scope: Vec<String>,
    duration: i64,
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenSubsetResponse {
    token: String,
}

pub fn token_subset(
    args: Json<TokenSubsetArgs>,
    state: State<AppState>,
    req: HttpRequest<AppState>
) -> HttpResponse {
    let new_exp = Utc::now().timestamp().saturating_add(i64::max(args.duration, 0));
    let maybe_claims = req.get_claims();
    if let Some(claims) = maybe_claims {
        if new_exp <= claims.exp &&
            tokens::sub_has_prefix (&args.sub, &claims.sub) &&
            args.scope.iter().all(|s| claims.scope.contains(s)) {
                let new_claims = Claims {
                    sub: args.sub.clone(),
                    scope: args.scope.clone(),
                    name: claims.name + "/" + &args.name,
                    exp: new_exp,
                };
                let token = jwt::encode(&jwt::Header::default(), &new_claims, &state.config.secret);
                // TODO: Check error and return token
                return HttpResponse::Ok().json(TokenSubsetResponse{ token: token.unwrap() });
            }
    };
    ApiError::NotEnoughPermissions.error_response()
}

#[derive(Deserialize)]
pub struct JobPathParams {
    id: i32,
}

pub fn get_job(
    params: Path<JobPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims("build", "jobs") {
        return From::from(e);
    }
    state
        .db
        .send(LookupBuild { id: params.id })
        .from_err()
        .and_then(|res| match res {
            Ok(job) => Ok(HttpResponse::Ok().json(job)),
            Err(e) => Ok(e.error_response())
        })
        .responder()
}

pub fn create_build(
    state: State<AppState>,
    req: HttpRequest<AppState>
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims("build", "build") {
        return From::from(e);
    }
    state
        .db
        .send(CreateBuild { })
        .from_err()
        .and_then(move |res| match res {
            Ok(build) => {
                match req.url_for("show_build", &[build.id.to_string()]) {
                    Ok(url) => Ok(HttpResponse::Ok()
                                  .header(http::header::LOCATION, url.to_string())
                                  .json(build)),
                    Err(e) => Ok(e.error_response())
                }
            },
            Err(e) => Ok(e.error_response())
        })
        .responder()
}


#[derive(Deserialize)]
pub struct BuildPathParams {
    id: i32,
}

pub fn get_build(
    params: Path<BuildPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "build") {
        return From::from(e);
    }
    state
        .db
        .send(LookupBuild { id: params.id })
        .from_err()
        .and_then(|res| match res {
            Ok(build) => Ok(HttpResponse::Ok().json(build)),
            Err(e) => Ok(e.error_response())
        })
        .responder()
}

#[derive(Deserialize)]
pub struct RefPathParams {
    id: i32,
    ref_id: i32,
}

pub fn get_build_ref(
    params: Path<RefPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "build") {
        return From::from(e);
    }
    state
        .db
        .send(LookupBuildRef {
            id: params.id,
            ref_id: params.ref_id,
        })
        .from_err()
        .and_then(|res| match res {
            Ok(build_ref) => Ok(HttpResponse::Ok().json(build_ref)),
            Err(e) => Ok(e.error_response())
        })
        .responder()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MissingObjectsArgs {
    wanted: Vec<String>
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MissingObjectsResponse {
    missing: Vec<String>
}

fn has_object (build_id: i32, object: &str, state: &State<AppState>) -> bool
{
    let subpath: path::PathBuf = ["objects", &object[..2], &object[2..]].iter().collect();
    let build_path = state.config.build_repo_base_path.join(build_id.to_string()).join(&subpath);
    if build_path.exists() {
        true
    } else {
        let main_path = state.config.repo_path.join(&subpath);
        main_path.exists()
    }
}

pub fn missing_objects(
    args: Json<MissingObjectsArgs>,
    params: Path<BuildPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> HttpResponse {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "upload") {
        return e.error_response();
    }
    let mut missing = vec![];
    for object in &args.wanted {
        if ! has_object (params.id, object, &state) {
            missing.push(object.to_string());
        }
    }
    HttpResponse::Ok()
        .content_encoding(http::ContentEncoding::Gzip)
        .json(MissingObjectsResponse { missing: missing })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateBuildRefArgs {
    #[serde(rename = "ref")] ref_name: String,
    commit: String,
}

pub fn create_build_ref (
    args: Json<CreateBuildRefArgs>,
    params: Path<BuildPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "upload") {
        return From::from(e);
    }
    state
        .db
        .send(CreateBuildRef {
            data: NewBuildRef {
                build_id: params.id,
                ref_name: args.ref_name.clone(),
                commit: args.commit.clone(),
            }
        })
        .from_err()
        .and_then(move |res| match res {
            Ok(buildref) =>  {
                match req.url_for("show_build_ref", &[params.id.to_string(), buildref.id.to_string()]) {
                    Ok(url) => Ok(HttpResponse::Ok()
                                  .header(http::header::LOCATION, url.to_string())
                                  .json(buildref)),
                    Err(e) => Ok(e.error_response())
                }
            },
            Err(e) => Ok(e.error_response())
        })
        .responder()
}

fn objectname_is_valid(name: &str) -> bool {
    let v: Vec<&str> = name.splitn(2, ".").collect();

    v.len() == 2 &&
        v[0].len() == 64  &&
        !v[0].contains(|c: char| !(c.is_digit(16) && !c.is_uppercase())) &&
        (v[1] == "dirmeta" ||
         v[1] == "dirtree" ||
         v[1] == "filez" ||
         v[1] == "commit")
}

fn get_object_name(field: &multipart::Field<dev::Payload>) -> error::Result<String, io::Error> {
    let cd = field.content_disposition().ok_or(
        io::Error::new(io::ErrorKind::InvalidInput,
                       "No content disposition for multipart item"))?;
    let filename = cd.get_filename().ok_or(
        io::Error::new(io::ErrorKind::InvalidInput,
                       "No filename for multipart item"))?;
    if !objectname_is_valid(filename) {
        Err(io::Error::new(io::ErrorKind::InvalidInput,
                           "Invalid object name"))
    } else {
        Ok(filename.to_string())
    }
}

struct UploadState {
    repo_path: path::PathBuf,
}

fn start_save(
    object: String,
    state: &Arc<UploadState>,
) -> Result<(NamedTempFile,path::PathBuf)> {
    let objects_dir = state.repo_path.join("objects");
    let object_dir = objects_dir.join(&object[..2]);
    let object_file = object_dir.join(&object[2..]);

    fs::create_dir_all(object_dir)?;

    let tmp_dir = state.repo_path.join("tmp");
    fs::create_dir_all(&tmp_dir)?;

    let named_file = NamedTempFile::new_in(&tmp_dir)?;
    Ok((named_file, object_file))
}

fn save_file(
    field: multipart::Field<dev::Payload>,
    state: &Arc<UploadState>
) -> Box<Future<Item = i64, Error = error::Error>> {
    let object = match  get_object_name (&field) {
        Ok(name) => name,
        Err(e) => return Box::new(future::err(ErrorBadRequest(e))),
    };

    let (named_file, object_file) = match start_save (object, state) {
        Ok((named_file, object_file)) => (named_file, object_file),
        Err(e) => return Box::new(future::err(error::ErrorInternalServerError(e))),
    };

    // We need file in two continuations below, so put it in a Rc+RefCell
    let shared_file = Rc::new(RefCell::new(named_file));
    let shared_file2 = shared_file.clone();
    Box::new(
        field
            .fold(0i64, move |acc, bytes| {
                let rt = shared_file.borrow_mut()
                    .write_all(bytes.as_ref())
                    .map(|_| acc + bytes.len() as i64)
                    .map_err(|e| {
                        println!("file.write_all failed: {:?}", e);
                        error::MultipartError::Payload(error::PayloadError::Io(e))
                    });
                future::result(rt)
            })
            .map_err(|e| {
                println!("save_file failed, {:?}", e);
                error::ErrorInternalServerError(e)
            }).and_then (move |res| {
                // persist consumes the named file, so we need to
                // completely move it out of the shared Rc+RefCell
                let named_file = Rc::try_unwrap(shared_file2).unwrap().into_inner();
                match named_file.persist(object_file) {
                    Ok(_persisted_file) => future::result(Ok(res)),
                    Err(e) => future::err(error::ErrorInternalServerError(e))
                }
            }),
    )
}

fn handle_multipart_item(
    item: multipart::MultipartItem<dev::Payload>,
    state: &Arc<UploadState>
) -> Box<Stream<Item = i64, Error = error::Error>> {
    match item {
        multipart::MultipartItem::Field(field) => {
            Box::new(save_file(field, state).into_stream())
        }
        multipart::MultipartItem::Nested(mp) => {
            let s = state.clone();
            Box::new(mp.map_err(error::ErrorInternalServerError)
                     .map(move |item| { handle_multipart_item (item, &s) })
                     .flatten())
        }
    }
}

pub fn upload(
    params: Path<BuildPathParams>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "upload") {
        return From::from(e);
    }
    let state = req.state();
    let uploadstate = Arc::new(UploadState { repo_path: state.config.build_repo_base_path.join(params.id.to_string()).join("upload") });
    Box::new(
        req.multipart()
            .map_err(error::ErrorInternalServerError)
            .map(move |item| { handle_multipart_item (item, &uploadstate) })
            .flatten()
            .collect()
            .map(|sizes| HttpResponse::Ok().json(sizes))
            .map_err(|e| {
                println!("failed: {}", e);
                e
            }),
    )
}

#[derive(Deserialize)]
pub struct CommitArgs {
    endoflife: Option<String>,
}

pub fn commit(
    args: Json<CommitArgs>,
    params: Path<BuildPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "build") {
        return From::from(e);
    }
    let tx = state.job_tx_channel.clone();
    state
        .db
        .send(StartCommitJob {
            id: params.id,
            endoflife: args.endoflife.clone(),
        })
        .from_err()
        .and_then(move |res| match res {
            Ok(build) => {
                tx.send(()).unwrap();
                match req.url_for("show_build", &[params.id.to_string()]) {
                    Ok(url) => Ok(HttpResponse::Ok()
                                  .header(http::header::LOCATION, url.to_string())
                                  .json(build)),
                    Err(e) => Ok(e.error_response())
                }
            },
            Err(e) => Ok(e.error_response())
        })
        .responder()
}

pub fn publish(
    params: Path<BuildPathParams>,
    state: State<AppState>,
    req: HttpRequest<AppState>,
) -> FutureResponse<HttpResponse> {
    if let Err(e) = req.has_token_claims(&format!("build/{}", params.id), "publish") {
        return From::from(e);
    }

    let tx = state.job_tx_channel.clone();
    state
        .db
        .send(StartPublishJob {
            id: params.id,
        })
        .from_err()
        .and_then(move |res| match res {
            Ok(build) => {
                tx.send(()).unwrap();
                match req.url_for("show_build", &[params.id.to_string()]) {
                    Ok(url) => Ok(HttpResponse::Ok()
                                  .header(http::header::LOCATION, url.to_string())
                                  .json(build)),
                    Err(e) => Ok(e.error_response())
                }
            },
            Err(e) => Ok(e.error_response())
        })
        .responder()
}