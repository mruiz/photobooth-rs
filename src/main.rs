use tokio::sync::RwLock;
use tokio::task;
use tonic::{transport::Server, Response, Status};
use proto::photo_booth_server::{PhotoBooth, PhotoBoothServer};
use gphoto2::{Camera, Context};
use gphoto2::widget::{RadioWidget, ToggleWidget};
use std::sync::Arc;
use tokio::sync::Notify;
use std::time::Duration;
use gphoto2::camera::CameraEvent;
use tokio::sync::Mutex;
use axum::{
    body:: {Body, Bytes},
    Router,
    routing::get,
    response::{IntoResponse, Response as AxumResponse},
    http::{
        StatusCode,
        header::CONTENT_TYPE
    }
};
use tower::ServiceBuilder;
use tokio::net::TcpListener;
use gpiod::{Chip, Options};


pub mod proto {
    tonic::include_proto!("photobooth");
    pub (crate) const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("photobooth_server_descriptor");
}

pub struct WrappedContext(Arc<Context>);
pub struct WrappedCamera(Arc<tokio::sync::RwLock<Option<Camera>>>);

pub struct MyPhotobooth {
    context: WrappedContext,
    camera: WrappedCamera,
    stop_preview_notify: Arc<Notify>,
    shared_preview: Arc<Mutex<Vec<u8>>>,
    new_image_notify: Arc<Notify>,
    shared_image: Arc<Mutex<Vec<u8>>>,
    previewing: Arc<Mutex<bool>>,
}

impl Default for WrappedContext {
    fn default() -> Self {
        WrappedContext(Arc::new(Context::new().unwrap()))
    }
}
impl WrappedContext {
    pub fn inner(&self) -> &Context {
        &self.0
    }
}

impl Default for WrappedCamera {
    fn default() -> Self {
        WrappedCamera(Arc::new(RwLock::new(None)))
    }
}

impl WrappedCamera {
    pub async fn inner(&self) -> Option<Camera> {
        self.0.read().await.clone()
    }

    pub async fn set(&self, camera: Camera) {
        let mut _camera = self.0.write().await;
        *_camera = Some(camera);
    }
}

impl MyPhotobooth {
    async fn shutdown_camera(&self) {
        let mut camera_lock = self.camera.0.write().await;

        if camera_lock.is_some() {
            println!("🛑 Stop preview and close camera");
            *camera_lock = None; // Drop de l'objet Camera → gp_camera_exit() implicite
        }
    }

    pub async fn get_preview_image(&self) -> Option<Vec<u8>> {
        let image = self.shared_preview.lock().await;
        if image.is_empty() {
            None
        } else {
            Some(image.clone())
        }
    }

    pub async fn get_capture_image(&self) -> Option<Vec<u8>> {
        let image = self.shared_image.lock().await;
        if image.is_empty() {
            None
        } else {
            Some(image.clone())
        }
    }

    pub async fn get_status_previewing(&self) -> bool {
        let status = self.previewing.lock().await;
        status.clone()
    }

    pub async fn set_status_previewing(&self, status: bool) {
        let mut previewing = self.previewing.lock().await;
        *previewing = status;
    }
}

impl Default for MyPhotobooth {
    fn default() -> Self {
        Self {
            context: WrappedContext::default(),
            camera: WrappedCamera::default(),
            stop_preview_notify: Arc::new(Notify::new()),
            shared_preview: Arc::new(Mutex::new(Vec::new())),
            new_image_notify: Arc::new(Notify::new()),
            shared_image: Arc::new(Mutex::new(Vec::new())),
            previewing: Arc::new(Mutex::new(false)),
        }
    }
}

#[tonic::async_trait]
impl PhotoBooth for MyPhotobooth {
    async fn list_camera(&self, _request: tonic::Request<()>) -> Result<Response<proto::ListCameraResponse>, Status> {
        let cameras: Vec<String> = {
            let mut camera_stream = self.context.inner().list_cameras()
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            
            let mut cameras = Vec::new();
            while let Some(camera_result) = camera_stream.next() {
                cameras.push(camera_result.model);
            }
            cameras
        };

        Ok(Response::new(proto::ListCameraResponse { cameras }))
    }

    async fn start_preview(&self, _request: tonic::Request<()>) -> Result<tonic::Response<()>, Status> {
        let img_clone = Arc::clone(&self.shared_preview);
        let ctx_clone = self.context.inner().clone();
        let notify = Arc::clone(&self.stop_preview_notify);
        let new_image_notify = Arc::clone(&self.new_image_notify);
        self.set_status_previewing(true).await;
        let previewing_clone = Arc::clone(&self.previewing);

        if !self.camera.inner().await.is_some() {
            let camera = self.context.inner().autodetect_camera().await.map_err(|e| Status::internal(e.to_string()))?;
            self.camera.set(camera).await;
        }

        match self.camera.inner().await {
            Some(camera) => {
                task::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = notify.notified() => {
                                println!("🛑 Stop streaming preview");
                                let mut previewing = previewing_clone.lock().await;
                                *previewing = false; 
                                break;
                            },
                            result = camera.capture_preview() => {
                                match result {
                                    Ok(preview_file) => match preview_file.get_data(&ctx_clone).await {
                                        Ok(data) => {
                                            let mut img = img_clone.lock().await;
                                            *img = data.into_vec();
                                            new_image_notify.notify_waiters();
                                        }
                                        Err(e) => eprintln!("Error capture_preview: {}", e),
                                    }
                                    Err(e) => eprintln!("❌ Error capture_preview: {}", e),
                                    
                                }
                                std::thread::sleep(Duration::from_millis(33)); // ~30 fps
                            }
                        }
                    }
                });
                Ok(Response::new(()))
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }

    
    async fn stop_preview(&self, _request: tonic::Request<()>) -> Result<tonic::Response<()>, Status> {
        if self.get_status_previewing().await == true {
            self.stop_preview_notify.notify_one();
            self.shutdown_camera().await;
        }
        
        Ok(Response::new(()))
    }

    async fn capture_image(&self, _request: tonic::Request<()>) -> Result<tonic::Response<()>, Status> {
        if !self.camera.inner().await.is_some() {
            let camera = self.context.inner().autodetect_camera().await.map_err(|e| Status::internal(e.to_string()))?;
            self.camera.set(camera).await;
        }

        let img_clone = Arc::clone(&self.shared_image);
        match self.camera.inner().await {
            Some(camera) => {
                let radio_widget = camera.config_key::<RadioWidget>("eosremoterelease").await.map_err(|e| Status::internal(e.to_string()))?;
                radio_widget.set_choice("Immediate").map_err(|e| Status::internal(e.to_string()))?;
                camera.set_config(&radio_widget).await.map_err(|e| Status::internal(e.to_string()))?; 
                radio_widget.set_choice("Release Full").map_err(|e| Status::internal(e.to_string()))?;
                camera.set_config(&radio_widget).await.map_err(|e| Status::internal(e.to_string()))?; 
                
                let mut retry = 0;

                loop {
                    let event = camera.wait_event(Duration::from_secs(10)).await.map_err(|e| Status::internal(e.to_string()))?;
                    if let CameraEvent::NewFile(file) = event {
                        let fs = camera.fs();
                        let camera_file = fs.download(&file.folder(), &file.name()).await.map_err(|e| Status::internal(e.to_string()))?;
                        let data = camera_file.get_data(self.context.inner()).await.map_err(|e| Status::internal(e.to_string()))?;
                        let mut img = img_clone.lock().await;
                        *img = data.into_vec();
                        fs.delete_file(&file.folder(), &file.name());
                        println!("New file: {}/{}", file.folder(), file.name());
                        break;
                    }

                    if let CameraEvent::Timeout = event {
                        return Err(Status::failed_precondition("Timeout No new file added"));
                    }

                    retry += 1;

                    if retry > 1000 {
                        return Err(Status::failed_precondition("No new file added"));
                    }
                }
                
                Ok(Response::new(()))
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }

    async fn config_radio_widget(&self, _request: tonic::Request<proto::ConfigRadioWidgetRequest>) -> Result<tonic::Response<()>, Status> {
        if !self.camera.inner().await.is_some() {
            let camera = self.context.inner().autodetect_camera().await.map_err(|e| Status::internal(e.to_string()))?;
            self.camera.set(camera).await;
        }

        let name = &_request.get_ref().name;
        let value = &_request.get_ref().value;
        match self.camera.inner().await {
            Some(camera) => {
                let radio_widget = camera.config_key::<RadioWidget>(&name).await.map_err(|e| Status::internal(format!("Failed to get {}: {}", name, e)))?;
                radio_widget.set_choice(&value).map_err(|e| Status::internal(format!("Failed to set {}: {}",name, e)))?;
                camera.set_config(&radio_widget).await.map_err(|e| Status::internal(format!("Failed to set config {} target: {}", name, e)))?; 
                Ok(Response::new(()))
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }

    async fn config_toggle_widget(&self, _request: tonic::Request<proto::ConfigToggleWidgetRequest>) -> Result<tonic::Response<()>, Status> {
        if !self.camera.inner().await.is_some() {
            let camera = self.context.inner().autodetect_camera().await.map_err(|e| Status::internal(e.to_string()))?;
            self.camera.set(camera).await;
        }

        let name = &_request.get_ref().name;
        let value = &_request.get_ref().value;
        match self.camera.inner().await {
            Some(camera) => {
                let toggle_widget = camera.config_key::<ToggleWidget>(&name).await.map_err(|e| Status::internal(format!("Failed to get key: {}", e)))?;
                toggle_widget.set_toggled(*value);
                camera.set_config(&toggle_widget).await.map_err(|e| Status::internal(format!("Failed to set config target: {}", e)))?; 
                Ok(Response::new(()))
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }
    
    async fn set_gpio(&self, _request: tonic::Request<proto::SetGpioRequest>) -> Result<tonic::Response<()>, Status> {
        let request_inner = _request.into_inner(); 
        let chip_req = request_inner.chip;
        let value_req = request_inner.value;
        let line_req = request_inner.line;

        let chip = Chip::new(chip_req).map_err(|e| Status::internal(format!("Failed to get key: {}", e)))?;
        let opts = Options::output([line_req])
            .values([false])
            .consumer("photobooth");
        let lines = chip.request_lines(opts).map_err(|e| Status::internal(format!("Failed to get key: {}", e)))?;

        lines.set_values(&[value_req]).map_err(|e| Status::internal(format!("Failed to get key: {}", e)))?;
        Ok(Response::new(()))
    }
}

#[tonic::async_trait]
impl PhotoBooth for Arc<MyPhotobooth> {
    async fn list_camera(&self, req: tonic::Request<()>) -> Result<tonic::Response<proto::ListCameraResponse>, tonic::Status> {
        self.as_ref().list_camera(req).await
    }

    async fn start_preview(&self, req: tonic::Request<()>) -> Result<tonic::Response<()>, tonic::Status> {
        self.as_ref().start_preview(req).await
    }

    async fn stop_preview(&self, req: tonic::Request<()>) -> Result<tonic::Response<()>, tonic::Status> {
        self.as_ref().stop_preview(req).await
    }

    async fn capture_image(&self, req: tonic::Request<()>) -> Result<tonic::Response<()>, tonic::Status> {
        self.as_ref().capture_image(req).await
    }

    async fn config_radio_widget(&self, req: tonic::Request<proto::ConfigRadioWidgetRequest>) -> Result<tonic::Response<()>, tonic::Status> {
        self.as_ref().config_radio_widget(req).await
    }

    async fn config_toggle_widget(&self, req: tonic::Request<proto::ConfigToggleWidgetRequest>) -> Result<tonic::Response<()>, tonic::Status> {
        self.as_ref().config_toggle_widget(req).await
    }

    async fn set_gpio(&self, req: tonic::Request<proto::SetGpioRequest>) -> Result<tonic::Response<()>, Status> {
        self.as_ref().set_gpio(req).await
    }
}

async fn mjpeg_preview_handler(
    photobooth: Arc<MyPhotobooth>,
) -> impl IntoResponse {
    // Boundary for MJPEG stream
    let boundary = "frame";

    let notify = Arc::clone(&photobooth.new_image_notify);
    // Créer le flux MJPEG
    let stream = async_stream::stream! {
        loop {
            let image_data = match photobooth.get_preview_image().await {
                Some(data) => data,
                None => {
                    notify.notified().await;
                    continue;
                }
            };

            let part = format!(
                "--{}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                boundary,
                image_data.len()
            )
            .into_bytes();

            yield Ok::<_, std::io::Error>(Bytes::from(part));
            yield Ok::<_, std::io::Error>(Bytes::from(image_data));
            yield Ok::<_, std::io::Error>(Bytes::from(b"\r\n".to_vec()));

            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    };

    // Créer le body en utilisant axum::body::Body
    let body = Body::from_stream(stream);

    // Construction de la réponse avec le bon Content-Type pour MJPEG
    AxumResponse::builder()
        .status(StatusCode::OK)
        .header(
            CONTENT_TYPE,
            format!("multipart/x-mixed-replace; boundary={}", boundary),
        )
        .body(body)
        .unwrap()
}

async fn image_handler(photobooth: Arc<MyPhotobooth>) -> AxumResponse {
    match photobooth.get_capture_image().await {
        Some(image_data) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "image/jpeg")],
            image_data,
        ).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "No preview image available".to_string(),
        ).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // let addr = "[::1]:50051".parse().unwrap();
    let grpc_addr = "[::]:50051".parse()?;
    let photobooth = Arc::new(MyPhotobooth::default());
    

    let http_server = {
        let app = Router::new()
            .route("/image", get({
                let photobooth = Arc::clone(&photobooth);
                move || image_handler(photobooth)
            }))
            .route("/preview", get({
                let photobooth = Arc::clone(&photobooth);
                move || mjpeg_preview_handler(photobooth)
            }))
            .layer(ServiceBuilder::new());

        let listener = TcpListener::bind("0.0.0.0:8080").await?;
        println!("🌐 HTTP server listening on http://{}", listener.local_addr()?);
        axum::serve(listener, app)
    };

    // let photobooth_sig = Arc::clone(&photobooth);
    // tokio::spawn(async move {
        
    //     let mut sigterm = signal(SignalKind::terminate()).unwrap();
        
    //     tokio::select! {
    //         _ = sigterm.recv() => {
    //             // Exécuter le nettoyage de la caméra
    //             println!("Received SIGTERM, shutting down...");
    //             photobooth_sig.shutdown_camera().await;
    //         }
    //     }
    // });

    let grpc_server = {
        let service = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
            .build_v1()?;
        Server::builder()
            .add_service(service)
            .add_service(PhotoBoothServer::new(Arc::clone(&photobooth)))
            .serve(grpc_addr)
    };


    tokio::select! {
        res = grpc_server => {
            if let Err(e) = res {
                eprintln!("gRPC server error: {}", e);
            }
        }
        res = http_server => {
            if let Err(e) = res {
                eprintln!("HTTP server error: {}", e);
            }
        }
    }

    Ok(())
}
