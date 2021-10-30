use std::{
    cell::RefCell, fmt, future::Future, marker::PhantomData, pin::Pin, rc::Rc, task,
};

use crate::http::Request;
use crate::router::ResourceDef;
use crate::service::boxed::{self, BoxServiceFactory};
use crate::service::{map_config, pipeline_factory, PipelineFactory};
use crate::service::{Identity, IntoServiceFactory, Service, ServiceFactory, Transform};
use crate::util::{Extensions, Ready};

use super::app_service::{AppFactory, AppService};
use super::config::{AppConfig, ServiceConfig};
use super::request::WebRequest;
use super::resource::Resource;
use super::response::WebResponse;
use super::route::Route;
use super::service::{AppServiceFactory, ServiceFactoryWrapper, WebServiceFactory};
use super::types::data::{Data, DataFactory};
use super::{DefaultError, ErrorRenderer};

type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, ()>;
type FnDataFactory =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn DataFactory>, ()>>>>>;

/// Application builder - structure that follows the builder pattern
/// for building application instances.
pub struct App<M, F, Err: ErrorRenderer = DefaultError> {
    middleware: M,
    filter: PipelineFactory<F>,
    services: Vec<Box<dyn AppServiceFactory<Err>>>,
    default: Option<Rc<HttpNewService<Err>>>,
    data: Vec<Box<dyn DataFactory>>,
    data_factories: Vec<FnDataFactory>,
    external: Vec<ResourceDef>,
    extensions: Extensions,
    error_renderer: Err,
    case_insensitive: bool,
}

impl App<Identity, Filter<DefaultError>, DefaultError> {
    /// Create application builder. Application can be configured with a builder-like pattern.
    pub fn new() -> Self {
        App {
            middleware: Identity,
            filter: pipeline_factory(Filter::new()),
            data: Vec::new(),
            data_factories: Vec::new(),
            services: Vec::new(),
            default: None,
            external: Vec::new(),
            extensions: Extensions::new(),
            error_renderer: DefaultError,
            case_insensitive: false,
        }
    }
}

impl<Err: ErrorRenderer> App<Identity, Filter<Err>, Err> {
    /// Create application builder with custom error renderer.
    pub fn with(err: Err) -> Self {
        App {
            middleware: Identity,
            filter: pipeline_factory(Filter::new()),
            data: Vec::new(),
            data_factories: Vec::new(),
            services: Vec::new(),
            default: None,
            external: Vec::new(),
            extensions: Extensions::new(),
            error_renderer: err,
            case_insensitive: false,
        }
    }
}

impl<M, T, Err> App<M, T, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    T::Future: 'static,
    Err: ErrorRenderer,
{
    /// Set application data. Application data could be accessed
    /// by using `Data<T>` extractor where `T` is data type.
    ///
    /// **Note**: http server accepts an application factory rather than
    /// an application instance. Http server constructs an application
    /// instance for each thread, thus application data must be constructed
    /// multiple times. If you want to share data between different
    /// threads, a shared object should be used, e.g. `Arc`. Internally `Data` type
    /// uses `Arc` so data could be created outside of app factory and clones could
    /// be stored via `App::app_data()` method.
    ///
    /// ```rust
    /// use std::cell::Cell;
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// struct MyData {
    ///     counter: Cell<usize>,
    /// }
    ///
    /// async fn index(data: web::types::Data<MyData>) -> HttpResponse {
    ///     data.counter.set(data.counter.get() + 1);
    ///     HttpResponse::Ok().into()
    /// }
    ///
    /// let app = App::new()
    ///     .data(MyData{ counter: Cell::new(0) })
    ///     .service(
    ///         web::resource("/index.html").route(web::get().to(index))
    ///     );
    /// ```
    pub fn data<U: 'static>(mut self, data: U) -> Self {
        self.data.push(Box::new(Data::new(data)));
        self
    }

    /// Set application data factory. This function is
    /// similar to `.data()` but it accepts data factory. Data object get
    /// constructed asynchronously during application initialization.
    pub fn data_factory<F, Out, D, E>(mut self, data: F) -> Self
    where
        F: Fn() -> Out + 'static,
        Out: Future<Output = Result<D, E>> + 'static,
        D: 'static,
        E: std::fmt::Debug,
    {
        self.data_factories.push(Box::new(move || {
            let fut = data();
            Box::pin(async move {
                match fut.await {
                    Err(e) => {
                        log::error!("Cannot construct data instance: {:?}", e);
                        Err(())
                    }
                    Ok(data) => {
                        let data: Box<dyn DataFactory> = Box::new(Data::new(data));
                        Ok(data)
                    }
                }
            })
        }));
        self
    }

    /// Set application level arbitrary data item.
    ///
    /// Application data stored with `App::app_data()` method is available
    /// via `HttpRequest::app_data()` method at runtime.
    ///
    /// This method could be used for storing `Data<T>` as well, in that case
    /// data could be accessed by using `Data<T>` extractor.
    pub fn app_data<U: 'static>(mut self, ext: U) -> Self {
        self.extensions.insert(ext);
        self
    }

    /// Run external configuration as part of the application building
    /// process
    ///
    /// This function is useful for moving parts of configuration to a
    /// different module or even library. For example,
    /// some of the resource's configuration could be moved to different module.
    ///
    /// ```rust
    /// use ntex::web::{self, middleware, App, HttpResponse};
    ///
    /// // this function could be located in different module
    /// fn config(cfg: &mut web::ServiceConfig) {
    ///     cfg.service(web::resource("/test")
    ///         .route(web::get().to(|| async { HttpResponse::Ok() }))
    ///         .route(web::head().to(|| async { HttpResponse::MethodNotAllowed() }))
    ///     );
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .wrap(middleware::Logger::default())
    ///         .configure(config)  // <- register resources
    ///         .route("/index.html", web::get().to(|| async { HttpResponse::Ok() }));
    /// }
    /// ```
    pub fn configure<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ServiceConfig<Err>),
    {
        let mut cfg = ServiceConfig::new();
        f(&mut cfg);
        self.data.extend(cfg.data);
        self.services.extend(cfg.services);
        self.external.extend(cfg.external);
        self
    }

    /// Configure route for a specific path.
    ///
    /// This is a simplified version of the `App::service()` method.
    /// This method can be used multiple times with same path, in that case
    /// multiple resources with one route would be registered for same resource path.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// async fn index(data: web::types::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .route("/test1", web::get().to(index))
    ///         .route("/test2", web::post().to(|| async { HttpResponse::MethodNotAllowed() }));
    /// }
    /// ```
    pub fn route(self, path: &str, mut route: Route<Err>) -> Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Register http service.
    ///
    /// Http service is any type that implements `WebServiceFactory` trait.
    ///
    /// ntex provides several services implementations:
    ///
    /// * *Resource* is an entry in resource table which corresponds to requested URL.
    /// * *Scope* is a set of resources with common root path.
    /// * "StaticFiles" is a service for static files support
    pub fn service<F>(mut self, factory: F) -> Self
    where
        F: WebServiceFactory<Err> + 'static,
    {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    /// Default service to be used if no matching resource could be found.
    ///
    /// It is possible to use services like `Resource`, `Route`.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(
    ///             web::resource("/index.html").route(web::get().to(index)))
    ///         .default_service(
    ///             web::route().to(|| async { HttpResponse::NotFound() }));
    /// }
    /// ```
    ///
    /// It is also possible to use static files as default service.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(
    ///             web::resource("/index.html").to(|| async { HttpResponse::Ok() }))
    ///         .default_service(
    ///             web::to(|| async { HttpResponse::NotFound() })
    ///         );
    /// }
    /// ```
    pub fn default_service<F, U>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<U>,
        U: ServiceFactory<
                Config = (),
                Request = WebRequest<Err>,
                Response = WebResponse,
                Error = Err::Container,
            > + 'static,
        U::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Some(Rc::new(boxed::factory(f.into_factory().map_init_err(
            |e| log::error!("Cannot construct default service: {:?}", e),
        ))));

        self
    }

    /// Register an external resource.
    ///
    /// External resources are useful for URL generation purposes only
    /// and are never considered for matching at request time. Calls to
    /// `HttpRequest::url_for()` will work as expected.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse, Error};
    ///
    /// async fn index(req: HttpRequest) -> Result<HttpResponse, Error> {
    ///     let url = req.url_for("youtube", &["asdlkjqme"])?;
    ///     assert_eq!(url.as_str(), "https://youtube.com/watch/asdlkjqme");
    ///     Ok(HttpResponse::Ok().into())
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(web::resource("/index.html").route(
    ///             web::get().to(index)))
    ///         .external_resource("youtube", "https://youtube.com/watch/{video_id}");
    /// }
    /// ```
    pub fn external_resource<N, U>(mut self, name: N, url: U) -> Self
    where
        N: AsRef<str>,
        U: AsRef<str>,
    {
        let mut rdef = ResourceDef::new(url.as_ref());
        *rdef.name_mut() = name.as_ref().to_string();
        self.external.push(rdef);
        self
    }

    /// Register request filter.
    ///
    /// Filter runs during inbound processing in the request
    /// lifecycle (request -> response), modifying request as
    /// necessary, across all requests managed by the *Application*.
    ///
    /// Use filter when you need to read or modify *every* request in some way.
    /// If filter returns request object then pipeline execution continues
    /// to the next service in pipeline. In case of response, it get returned
    /// immediately.
    ///
    /// ```rust
    /// use ntex::http::header::{CONTENT_TYPE, HeaderValue};
    /// use ntex::web::{self, middleware, App};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .wrap(middleware::Logger::default())
    ///         .route("/index.html", web::get().to(index));
    /// }
    /// ```
    pub fn filter<S, U>(
        self,
        filter: U,
    ) -> App<
        M,
        impl ServiceFactory<
            Config = (),
            Request = WebRequest<Err>,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
        Err,
    >
    where
        S: ServiceFactory<
            Config = (),
            Request = WebRequest<Err>,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
        U: IntoServiceFactory<S>,
    {
        App {
            filter: self.filter.and_then(filter.into_factory()),
            middleware: self.middleware,
            data: self.data,
            data_factories: self.data_factories,
            services: self.services,
            default: self.default,
            external: self.external,
            extensions: self.extensions,
            error_renderer: self.error_renderer,
            case_insensitive: self.case_insensitive,
        }
    }

    /// Registers middleware, in the form of a middleware component (type),
    /// that runs during inbound and/or outbound processing in the request
    /// lifecycle (request -> response), modifying request/response as
    /// necessary, across all requests managed by the *Application*.
    ///
    /// Use middleware when you need to read or modify *every* request or
    /// response in some way.
    ///
    /// Notice that the keyword for registering middleware is `wrap`. As you
    /// register middleware using `wrap` in the App builder,  imagine wrapping
    /// layers around an inner App.
    ///
    /// ```rust
    /// use ntex::http::header::{CONTENT_TYPE, HeaderValue};
    /// use ntex::web::{self, middleware, App};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .wrap(middleware::Logger::default())
    ///         .route("/index.html", web::get().to(index));
    /// }
    /// ```
    pub fn wrap<U>(self, mw: U) -> App<Stack<M, U>, T, Err> {
        App {
            middleware: Stack::new(self.middleware, mw),
            filter: self.filter,
            data: self.data,
            data_factories: self.data_factories,
            services: self.services,
            default: self.default,
            external: self.external,
            extensions: self.extensions,
            error_renderer: self.error_renderer,
            case_insensitive: self.case_insensitive,
        }
    }

    /// Use ascii case-insensitive routing.
    ///
    /// Only static segments could be case-insensitive.
    pub fn case_insensitive_routing(mut self) -> Self {
        self.case_insensitive = true;
        self
    }
}

impl<M, F, Err> App<M, F, Err>
where
    M: Transform<AppService<F::Service, Err>> + 'static,
    M::Service: Service<
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
    >,
    F: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    F::Future: 'static,
    Err: ErrorRenderer,
{
    /// Construct service factory with default `AppConfig`, suitable for `http::HttpService`.
    ///
    /// ```rust,no_run
    /// use ntex::{web, http, server};
    ///
    /// #[ntex::main]
    /// async fn main() -> std::io::Result<()> {
    ///     server::build().bind("http", "127.0.0.1:0", ||
    ///         http::HttpService::build().finish(
    ///             web::App::new()
    ///                 .route("/index.html", web::get().to(|| async { "hello_world" }))
    ///                 .finish()
    ///         ).tcp()
    ///     )?
    ///     .run()
    ///     .await
    /// }
    /// ```
    pub fn finish(
        self,
    ) -> impl ServiceFactory<
        Config = (),
        Request = Request,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    > {
        map_config(self.into_factory(), move |_| Default::default())
    }

    /// Construct service factory suitable for `http::HttpService`.
    ///
    /// ```rust,no_run
    /// use ntex::{web, http, server};
    ///
    /// #[ntex::main]
    /// async fn main() -> std::io::Result<()> {
    ///     server::build().bind("http", "127.0.0.1:0", ||
    ///         http::HttpService::build().finish(
    ///             web::App::new()
    ///                 .route("/index.html", web::get().to(|| async { "hello_world" }))
    ///                 .with_config(web::dev::AppConfig::default())
    ///         ).tcp()
    ///     )?
    ///     .run()
    ///     .await
    /// }
    /// ```
    pub fn with_config(
        self,
        cfg: AppConfig,
    ) -> impl ServiceFactory<
        Config = (),
        Request = Request,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    > {
        map_config(self.into_factory(), move |_| cfg.clone())
    }
}

impl<M, F, Err> IntoServiceFactory<AppFactory<M, F, Err>> for App<M, F, Err>
where
    M: Transform<AppService<F::Service, Err>> + 'static,
    M::Service: Service<
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
    >,
    F: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    F::Future: 'static,
    Err: ErrorRenderer,
{
    fn into_factory(self) -> AppFactory<M, F, Err> {
        AppFactory {
            filter: self.filter,
            middleware: Rc::new(self.middleware),
            data: Rc::new(self.data),
            data_factories: Rc::new(self.data_factories),
            services: Rc::new(RefCell::new(self.services)),
            external: RefCell::new(self.external),
            default: self.default,
            extensions: RefCell::new(Some(self.extensions)),
            case_insensitive: self.case_insensitive,
        }
    }
}

pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer> Stack<Inner, Outer> {
    pub(super) fn new(inner: Inner, outer: Outer) -> Self {
        Stack { inner, outer }
    }
}

impl<S, Inner, Outer> Transform<S> for Stack<Inner, Outer>
where
    Inner: Transform<S>,
    Outer: Transform<Inner::Service>,
{
    type Service = Outer::Service;

    fn new_transform(&self, service: S) -> Self::Service {
        self.outer.new_transform(self.inner.new_transform(service))
    }
}

pub struct Filter<Err>(PhantomData<Err>);

impl<Err: ErrorRenderer> Filter<Err> {
    pub(super) fn new() -> Self {
        Filter(PhantomData)
    }
}

impl<Err: ErrorRenderer> ServiceFactory for Filter<Err> {
    type Config = ();
    type Request = WebRequest<Err>;
    type Response = WebRequest<Err>;
    type Error = Err::Container;
    type InitError = ();
    type Service = Filter<Err>;
    type Future = Ready<Filter<Err>, ()>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(Filter(PhantomData))
    }
}

impl<Err: ErrorRenderer> Service for Filter<Err> {
    type Request = WebRequest<Err>;
    type Response = WebRequest<Err>;
    type Error = Err::Container;
    type Future = Ready<WebRequest<Err>, Err::Container>;

    #[inline]
    fn poll_ready(
        &self,
        _: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Self::Request) -> Self::Future {
        Ready::Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header::{self, HeaderValue};
    use crate::http::{Method, StatusCode};
    use crate::service::{fn_service, Service};
    use crate::util::{Ready};
    use crate::web::test::{call_service, init_service, TestRequest};
    use crate::web::{
        self, middleware::DefaultHeaders, request::WebRequest, DefaultError,
        HttpRequest, HttpResponse,
    };

    #[crate::rt_test]
    async fn test_default_resource() {
        let srv = init_service(
            App::new()
                .service(web::resource("/test").to(|| async { HttpResponse::Ok() })),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/blah").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let srv = init_service(
            App::new()
                .service(web::resource("/test").to(|| async { HttpResponse::Ok() }))
                .service(
                    web::resource("/test2")
                        .default_service(|r: WebRequest<DefaultError>| async move {
                            Ok(r.into_response(HttpResponse::Created()))
                        })
                        .route(web::get().to(|| async { HttpResponse::Ok() })),
                )
                .default_service(|r: WebRequest<DefaultError>| async move {
                    Ok(r.into_response(HttpResponse::MethodNotAllowed()))
                }),
        )
        .await;

        let req = TestRequest::with_uri("/blah").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test2")
            .method(Method::POST)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_data_factory() {
        let srv = init_service(
            App::new()
                .data_factory(|| async { Ok::<_, ()>(10usize) })
                .service(
                    web::resource("/")
                        .to(|_: web::types::Data<usize>| async { HttpResponse::Ok() }),
                ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::new()
                .data_factory(|| async { Ok::<_, ()>(10u32) })
                .service(
                    web::resource("/")
                        .to(|_: web::types::Data<usize>| async { HttpResponse::Ok() }),
                ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[crate::rt_test]
    async fn test_extension() {
        let srv = init_service(App::new().app_data(10usize).service(
            web::resource("/").to(|req: HttpRequest| async move {
                assert_eq!(*req.app_data::<usize>().unwrap(), 10);
                HttpResponse::Ok()
            }),
        ))
        .await;
        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_filter() {
        let filter = Rc::new(std::cell::Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new()
                .filter(fn_service(move |req: WebRequest<_>| {
                    filter2.set(true);
                    Ready::Ok(req)
                }))
                .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(filter.get());
    }

    #[crate::rt_test]
    async fn test_wrap() {
        let srv = init_service(
            App::new()
                .wrap(
                    DefaultHeaders::new()
                        .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
                )
                .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[crate::rt_test]
    async fn test_router_wrap() {
        let srv = init_service(
            App::new()
                .route("/test", web::get().to(|| async { HttpResponse::Ok() }))
                .wrap(
                    DefaultHeaders::new()
                        .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
                ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[crate::rt_test]
    async fn test_case_insensitive_router() {
        let srv = init_service(
            App::new()
                .case_insensitive_routing()
                .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/Test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "url")]
    #[crate::rt_test]
    async fn test_external_resource() {
        let srv = init_service(
            App::new()
                .external_resource("youtube", "https://youtube.com/watch/{video_id}")
                .route(
                    "/test",
                    web::get().to(|req: HttpRequest| async move {
                        HttpResponse::Ok().body(format!(
                            "{}",
                            req.url_for("youtube", &["12345"]).unwrap()
                        ))
                    }),
                ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        assert_eq!(body, Bytes::from_static(b"https://youtube.com/watch/12345"));
    }
}
