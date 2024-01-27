#![allow(clippy::type_complexity)]

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::{ok, LocalBoxFuture, Ready};
use futures::FutureExt;

use actix_service::{Service, Transform};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    Error, HttpMessage, HttpResponse, Result,
};

use casbin::prelude::{TryIntoAdapter, TryIntoModel};
use casbin::{CachedEnforcer, CoreApi, Result as CasbinResult};

#[cfg(feature = "runtime-tokio")]
use tokio::sync::RwLock;

#[cfg(feature = "runtime-async-std")]
use async_std::sync::RwLock;

#[derive(Clone)]
pub struct CasbinVals {
    pub subject: String,
    pub domain: Option<String>,
}

#[derive(Clone)]
pub struct CasbinService {
    enforcer: Arc<RwLock<CachedEnforcer>>,
}

impl CasbinService {
    pub async fn new<M: TryIntoModel, A: TryIntoAdapter>(m: M, a: A) -> CasbinResult<Self> {
        let enforcer: CachedEnforcer = CachedEnforcer::new(m, a).await?;
        Ok(CasbinService {
            enforcer: Arc::new(RwLock::new(enforcer)),
        })
    }

    pub fn get_enforcer(&mut self) -> Arc<RwLock<CachedEnforcer>> {
        self.enforcer.clone()
    }

    pub fn set_enforcer(e: Arc<RwLock<CachedEnforcer>>) -> CasbinService {
        CasbinService { enforcer: e }
    }
}

impl<S, B> Transform<S, ServiceRequest> for CasbinService
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = CasbinMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(CasbinMiddleware {
            enforcer: self.enforcer.clone(),
            service: Rc::new(RefCell::new(service)),
        })
    }
}

impl Deref for CasbinService {
    type Target = Arc<RwLock<CachedEnforcer>>;

    fn deref(&self) -> &Self::Target {
        &self.enforcer
    }
}

impl DerefMut for CasbinService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.enforcer
    }
}

pub struct CasbinMiddleware<S> {
    service: Rc<RefCell<S>>,
    enforcer: Arc<RwLock<CachedEnforcer>>,
}

impl<S, B> Service<ServiceRequest> for CasbinMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: MessageBody,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = S::Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let cloned_enforcer = self.enforcer.clone();
        let srv = self.service.clone();

        async move {
            let path = req.path().to_string();
            let action = req.method().as_str().to_string();
            let option_vals = req.extensions().get::<CasbinVals>().map(|x| x.to_owned());
            let vals = match option_vals {
                Some(value) => value,
                None => {
                    return Ok(req.into_response(
                        HttpResponse::Unauthorized().finish().map_into_right_body(),
                    ))
                }
            };
            let subject = vals.subject.clone();

            if !vals.subject.is_empty() {
                if let Some(domain) = vals.domain {
                    let mut lock = cloned_enforcer.write().await;
                    eprintln!("{subject:?}{domain:?}{path:?}{action:?}");
                    match lock.enforce_mut(vec![subject, domain, path, action]) {
                        Ok(true) => {
                            drop(lock);
                            srv.call(req).await.map(|res| res.map_into_left_body())
                        }
                        Ok(false) => {
                            drop(lock);
                            Ok(req.into_response(
                                HttpResponse::Forbidden().finish().map_into_right_body(),
                            ))
                        }
                        Err(err) => {
                            eprintln!("140 {err:?}");
                            drop(lock);
                            Ok(req.into_response(
                                HttpResponse::BadGateway().finish().map_into_right_body(),
                            ))
                        }
                    }
                } else {
                    let mut lock = cloned_enforcer.write().await;
                    match lock.enforce_mut(vec![subject, path, action]) {
                        Ok(true) => {
                            drop(lock);
                            srv.call(req).await.map(|res| res.map_into_left_body())
                        }
                        Ok(false) => {
                            drop(lock);
                            Ok(req.into_response(
                                HttpResponse::Forbidden().finish().map_into_right_body(),
                            ))
                        }
                        Err(err) => {
                            eprintln!("161 {err:?}");
                            drop(lock);
                            Ok(req.into_response(
                                HttpResponse::BadGateway().finish().map_into_right_body(),
                            ))
                        }
                    }
                }
            } else {
                Ok(req.into_response(HttpResponse::Unauthorized().finish().map_into_right_body()))
            }
        }
        .boxed_local()
    }
}
