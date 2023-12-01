use crate::error::Error;
use crate::jobs;
use crate::models;
use crate::nix;
use crate::responses;
use crate::schema;
use crate::tasks;
use crate::Conn;
use crate::DbPool;

use std::collections::HashMap;
use typhon_types::data::TaskStatusKind;
use typhon_types::*;

use diesel::prelude::*;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct Evaluation {
    pub task: tasks::Task,
    pub evaluation: models::Evaluation,
    pub project: models::Project,
}

#[ext_trait::extension(pub trait ExtraRunInfo)]
impl responses::RunInfo {
    fn new(
        job_handle: &handles::Job,
        run: models::Run,
        begin: Option<(models::Action, models::Task)>,
        build: Option<(models::Build, models::Task)>,
        end: Option<(models::Action, models::Task)>,
    ) -> Self {
        let to_action_info =
            |(action, task): (models::Action, models::Task)| responses::ActionInfo {
                handle: handles::Action {
                    project: job_handle.evaluation.project.clone(),
                    num: action.num as u64,
                },
                input: action.input,
                path: action.path,
                status: task.status(),
            };
        responses::RunInfo {
            handle: handles::Run {
                job: job_handle.clone(),
                num: run.num as u64,
            },
            begin: begin.map(to_action_info),
            build: build.map(|(build, task)| responses::BuildInfo {
                handle: handles::Build {
                    drv: build.drv.clone(),
                    num: build.num as u64,
                },
                drv: build.drv,
                status: task.status(),
            }),
            end: end.map(to_action_info),
        }
    }
}

#[ext_trait::extension(pub trait ExtraJobInfo)]
impl responses::JobInfo {
    /// Reshape raw database data into a structured `JobInfo`
    fn new(
        eval_handle: &handles::Evaluation,
        job: models::Job,
        run: models::Run,
        run_count: u32,
        begin: Option<(models::Action, models::Task)>,
        build: Option<(models::Build, models::Task)>,
        end: Option<(models::Action, models::Task)>,
    ) -> Self {
        let job_handle = handles::Job {
            evaluation: eval_handle.clone(),
            system: job.system.clone(),
            name: job.name.clone(),
        };
        Self {
            handle: job_handle.clone(),
            dist: job.dist,
            drv: job.drv,
            out: job.out,
            system: job.system,
            last_run: responses::RunInfo::new(&job_handle, run, begin, build, end),
            run_count,
        }
    }
}

impl Evaluation {
    pub fn cancel(&self) {
        self.task.cancel()
    }

    pub fn finish(
        self,
        r: Option<Result<nix::NewJobs, nix::Error>>,
        pool: &DbPool,
    ) -> TaskStatusKind {
        let mut conn = pool.get().unwrap();
        match r {
            Some(Ok(new_jobs)) => match self.create_new_jobs(&mut conn, new_jobs) {
                Ok(()) => TaskStatusKind::Success,
                Err(_) => TaskStatusKind::Error,
            },
            Some(Err(_)) => TaskStatusKind::Error,
            None => TaskStatusKind::Canceled,
        }
    }

    pub fn get(conn: &mut Conn, handle: &handles::Evaluation) -> Result<Self, Error> {
        let (evaluation, project, task) = schema::evaluations::table
            .inner_join(schema::projects::table)
            .inner_join(schema::tasks::table)
            .filter(schema::projects::name.eq(&handle.project.name))
            .filter(schema::evaluations::num.eq(handle.num as i64))
            .first(conn)
            .optional()?
            .ok_or(Error::EvaluationNotFound(handle.clone()))?;
        Ok(Self {
            task: tasks::Task { task },
            evaluation,
            project,
        })
    }

    pub fn handle(&self) -> handles::Evaluation {
        handles::evaluation((self.project.name.clone(), self.evaluation.num as u64))
    }

    /// Fetch all jobs attached to self
    pub fn jobs(
        eval_handle: &handles::Evaluation,
        eval_id: i32,
        filter_system_name: Option<responses::JobSystemName>,
        conn: &mut Conn,
    ) -> Result<HashMap<responses::JobSystemName, responses::JobInfo>, Error> {
        let (begin_action, end_action, begin_task, build_task, end_task, subruns) = diesel::alias!(
            schema::actions as begin_action,
            schema::actions as end_action,
            schema::tasks as begin_task,
            schema::tasks as build_task,
            schema::tasks as end_task,
            schema::runs as subruns,
        );
        let runs_per_job = schema::jobs::table
            .inner_join(schema::runs::table)
            .group_by(schema::runs::job_id)
            .select((schema::runs::job_id, diesel::dsl::count(schema::runs::id)))
            .load::<(i32, i64)>(conn)?;
        let runs_per_job: HashMap<i32, i64> = runs_per_job.iter().copied().collect();
        let mut query = schema::jobs::table
            .inner_join(schema::runs::table)
            .left_join(
                begin_action
                    .on(begin_action
                        .field(schema::actions::id)
                        .nullable()
                        .eq(schema::runs::begin_id))
                    .inner_join(begin_task),
            )
            .left_join(
                schema::builds::table
                    .on(schema::builds::id.nullable().eq(schema::runs::build_id))
                    .inner_join(build_task),
            )
            .left_join(
                end_action
                    .on(end_action
                        .field(schema::actions::id)
                        .nullable()
                        .eq(schema::runs::end_id))
                    .inner_join(end_task),
            )
            .filter(
                schema::runs::job_id.nullable().eq(subruns
                    .filter(subruns.field(schema::runs::job_id).eq(schema::jobs::id))
                    .group_by(subruns.field(schema::runs::job_id))
                    .select(diesel::dsl::max(subruns.field(schema::runs::id)))
                    .single_value()),
            )
            .filter(schema::jobs::evaluation_id.eq(eval_id))
            .into_boxed();
        if let Some(filter) = filter_system_name {
            query = query
                .filter(schema::jobs::system.eq(filter.system))
                .filter(schema::jobs::name.eq(filter.name));
        }
        Ok(query
            .select((
                schema::jobs::all_columns,
                schema::runs::all_columns,
                (
                    begin_action.fields(schema::actions::all_columns),
                    begin_task.fields(schema::tasks::all_columns),
                )
                    .nullable(),
                (
                    schema::builds::all_columns,
                    build_task.fields(schema::tasks::all_columns),
                )
                    .nullable(),
                (
                    end_action.fields(schema::actions::all_columns),
                    end_task.fields(schema::tasks::all_columns),
                )
                    .nullable(),
            ))
            .load(conn)?
            .into_iter()
            .map(
                |(job, run, begin, build, end): (models::Job, models::Run, _, _, _)| {
                    let run_count = runs_per_job.get(&run.id).copied().unwrap_or(1) as u32;
                    let (system, name) = (job.system.clone(), job.name.clone());
                    (
                        responses::JobSystemName { system, name },
                        responses::JobInfo::new(
                            &eval_handle,
                            job,
                            run,
                            run_count,
                            begin,
                            build,
                            end,
                        ),
                    )
                },
            )
            .collect())
    }

    pub fn info(&self, conn: &mut Conn) -> Result<responses::EvaluationInfo, Error> {
        Ok(responses::EvaluationInfo {
            handle: self.handle(),
            actions_path: self.evaluation.actions_path.clone(),
            flake: self.evaluation.flake,
            jobs: if self.task.status_kind() == TaskStatusKind::Success {
                Self::jobs(&self.handle(), self.evaluation.id, None, conn)?
            } else {
                HashMap::new()
            },
            jobset_name: self.evaluation.jobset_name.clone(),
            status: self.task.status(),
            time_created: time::OffsetDateTime::from_unix_timestamp(self.evaluation.time_created)?,
            url: self.evaluation.url.clone(),
        })
    }

    pub fn log(&self, conn: &mut Conn) -> Result<Option<String>, Error> {
        self.task.log(conn)
    }

    pub async fn run(
        self,
        sender: mpsc::UnboundedSender<String>,
    ) -> Result<nix::NewJobs, nix::Error> {
        let res = nix::eval_jobs(&self.evaluation.url, self.evaluation.flake).await;
        match &res {
            Err(e) => {
                for line in e.to_string().split("\n") {
                    // TODO: hide internal error messages?
                    // TODO: error management
                    let _ = sender.send(line.to_string());
                }
            }
            _ => (),
        }
        res
    }

    fn create_new_jobs(&self, conn: &mut Conn, new_jobs: nix::NewJobs) -> Result<(), Error> {
        let created_runs = conn.transaction::<Vec<crate::runs::Run>, Error, _>(|conn| {
            let created_jobs: Vec<crate::jobs::Job> = new_jobs
                .into_iter()
                .map(|((system, name), (drv, dist))| {
                    let new_job = models::NewJob {
                        dist,
                        drv: &drv.path.to_string(),
                        evaluation_id: self.evaluation.id,
                        name: &name,
                        out: drv
                            .outputs
                            .iter()
                            .last()
                            .expect("TODO: derivations can have multiple outputs")
                            .1,
                        system: &system,
                    };
                    let job = diesel::insert_into(schema::jobs::table)
                        .values(&new_job)
                        .get_result::<models::Job>(conn)?;
                    Ok(jobs::Job {
                        project: self.project.clone(),
                        evaluation: self.evaluation.clone(),
                        job,
                    })
                })
                .collect::<Result<_, Error>>()?;
            created_jobs
                .into_iter()
                .map(|job| job.new_run(conn))
                .collect()
        })?;

        for run in created_runs {
            run.run(conn)?;
        }

        Ok(())
    }
}
