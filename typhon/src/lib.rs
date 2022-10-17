mod actions;
mod builds;
mod error;
mod evaluations;
mod jobs;
mod jobsets;
mod models;
mod nix;
mod projects;
mod schema;
mod time;

pub mod api;
pub mod tasks;

pub use typhon_types::{handles, requests, responses, responses::Response, Event};

use error::Error;
use models::*;

use diesel::prelude::*;
use log::*;
use once_cell::sync::OnceCell;
use rocket::Responder;
use sha256;
use std::sync::Mutex;

#[derive(Debug)]
pub struct Settings {
    pub hashed_password: String,
    pub webroot: String,
}

// Typhon's state
pub static SETTINGS: OnceCell<Settings> = OnceCell::new();
pub static EVALUATIONS: OnceCell<tasks::Tasks<i32>> = OnceCell::new();
pub static BUILDS: OnceCell<tasks::Tasks<i32>> = OnceCell::new();
pub static JOBS: OnceCell<tasks::Tasks<i32>> = OnceCell::new();
pub static CONNECTION: OnceCell<Mutex<SqliteConnection>> = OnceCell::new();

pub fn connection<'a>() -> std::sync::MutexGuard<'a, SqliteConnection> {
    CONNECTION.get().unwrap().lock().unwrap()
}

#[derive(Debug, Clone, Copy)]
pub enum User {
    Admin,
    Anonymous,
}

impl User {
    pub fn is_admin(&self) -> bool {
        match self {
            User::Admin => true,
            _ => false,
        }
    }
}

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for User {
    type Error = ();
    async fn from_request(
        request: &'r rocket::Request<'_>,
    ) -> rocket::request::Outcome<Self, Self::Error> {
        rocket::request::Outcome::Success(
            //request
            //    .cookies()
            //    .get_private("admin")
            //    .map_or(User::Anonymous, |_| User::Admin),
            request
                .headers()
                .get("password")
                .last()
                .map_or(User::Anonymous, |password| {
                    let hash = sha256::digest(password);
                    if hash == SETTINGS.get().unwrap().hashed_password {
                        User::Admin
                    } else {
                        User::Anonymous
                    }
                }),
        )
    }
}

#[derive(Responder)]
pub enum ResponseError {
    #[response(status = 404)]
    ResourceNotFound(String),
    #[response(status = 500)]
    InternalError(()),
    #[response(status = 400)]
    BadRequest(String),
}

impl From<error::Error> for ResponseError {
    fn from(e: error::Error) -> ResponseError {
        use {error::Error::*, ResponseError::*};
        match e {
            BuildNotFound(_)
            | EvaluationNotFound(_)
            | JobNotFound(_)
            | JobsetNotFound(_)
            | ProjectNotFound(_) => ResourceNotFound(format!("{}", e)),
            AccessDenied
            | ActionError(_)
            | BadJobsetDecl(_)
            | BuildNotRunning(_)
            | EvaluationNotRunning(_)
            | NixError(_)
            | ProjectAlreadyExists(_) => BadRequest(format!("{}", e)),
            Todo | UnexpectedDatabaseError(_) => InternalError(()),
        }
    }
}

pub fn authorize_request(user: &User, req: &requests::Request) -> bool {
    match req {
        requests::Request::ListProjects
        | requests::Request::Project(_, requests::Project::Info)
        | requests::Request::Jobset(_, requests::Jobset::Info)
        | requests::Request::Evaluation(_, requests::Evaluation::Info)
        | requests::Request::Job(_, requests::Job::Info)
        | requests::Request::Build(_, requests::Build::Info) => true,
        _ => user.is_admin(),
    }
}

pub fn handle_request_aux(user: &User, req: &requests::Request) -> Result<Response, Error> {
    if authorize_request(user, req) {
        let conn = &mut *CONNECTION.get().unwrap().lock().unwrap();
        Ok(match req {
            requests::Request::ListProjects => Response::ListProjects(Project::list(conn)?),
            requests::Request::CreateProject(project_handle) => {
                Project::create(conn, &project_handle)?;
                Response::Ok
            }
            requests::Request::Project(project_handle, req) => {
                let project = Project::get(conn, &project_handle)?;
                match req {
                    requests::Project::Delete => {
                        project.delete(conn)?;
                        Response::Ok
                    }
                    requests::Project::Info => Response::ProjectInfo(project.info(conn)?),
                    requests::Project::Refresh => {
                        project.refresh(conn)?;
                        Response::Ok
                    }
                    requests::Project::SetDecl(flake) => {
                        project.set_decl(conn, &flake)?;
                        Response::Ok
                    }
                    requests::Project::SetPrivateKey(key) => {
                        project.set_private_key(conn, &key)?;
                        Response::Ok
                    }
                    requests::Project::UpdateJobsets => {
                        let jobsets = project.update_jobsets(conn)?;
                        Response::ProjectUpdateJobsets(jobsets)
                    }
                }
            }
            requests::Request::Jobset(jobset_handle, req) => {
                let jobset = Jobset::get(conn, &jobset_handle)?;
                match req {
                    requests::Jobset::Evaluate => {
                        let evaluation_handle = jobset.evaluate(conn)?;
                        Response::JobsetEvaluate(evaluation_handle)
                    }
                    requests::Jobset::Info => Response::JobsetInfo(jobset.info(conn)?),
                }
            }
            requests::Request::Evaluation(evaluation_handle, req) => {
                let evaluation = Evaluation::get(conn, evaluation_handle)?;
                match req {
                    requests::Evaluation::Cancel => {
                        evaluation.cancel()?;
                        Response::Ok
                    }
                    requests::Evaluation::Info => Response::EvaluationInfo(evaluation.info(conn)?),
                }
            }
            requests::Request::Job(job_handle, req) => {
                let job = Job::get(conn, &job_handle)?;
                match req {
                    requests::Job::Cancel => {
                        job.cancel()?;
                        Response::Ok
                    }
                    requests::Job::Info => Response::JobInfo(job.info(conn)?),
                }
            }
            requests::Request::Build(build_handle, req) => {
                let build = Build::get(conn, &build_handle)?;
                match req {
                    requests::Build::Cancel => {
                        build.cancel()?;
                        Response::Ok
                    }
                    requests::Build::Info => Response::BuildInfo(build.info()?),
                    requests::Build::Log => Response::BuildLog, // build.log()?
                }
            }
        })
    } else {
        Err(Error::AccessDenied)
    }
}

/// Main entry point for Typhon requests
pub fn handle_request(user: User, req: requests::Request) -> Result<Response, ResponseError> {
    info!("handling request {:?} for user {:?}", req, user);
    Ok(handle_request_aux(&user, &req).map_err(|e| {
        if e.is_internal() {
            error!(
                "request {:?} for user {:?} raised error: {:?}",
                req, user, e
            );
        }
        e
    })?)
}

pub fn log_event(event: Event) {
    info!("event: {:?}", event)
}
