use tokio::{sync::RwLock, time::timeout};
use tonic::{transport::Server, Response, Status};
use proto::photo_booth_server::{PhotoBooth, PhotoBoothServer};
use gphoto2::{Camera, Context};
use gphoto2::widget::{RadioWidget, ToggleWidget};
use std::{sync::Arc, time::Duration};

pub mod proto {
    tonic::include_proto!("photobooth");
    pub (crate) const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("photobooth_server_descriptor");
}

pub struct WrappedContext(Arc<Context>);
pub struct WrappedCamera(Arc<tokio::sync::RwLock<Option<Camera>>>);

#[derive(Default)]
pub struct MyPhotobooth {
    context: WrappedContext,
    camera: WrappedCamera
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
        Ok(Response::new(()))
    }

    async fn stop_preview(&self, _request: tonic::Request<()>) -> Result<tonic::Response<()>, Status> {
        Ok(Response::new(()))
    }

    async fn take_photo(&self, _request: tonic::Request<()>) -> Result<Response<proto::TakePhotoResponse>, Status> {
        match self.camera.inner().await {
            Some(camera) => {
                match timeout(Duration::from_secs(10), camera.capture_image()).await {
                    Ok(result) => match result {
                        Ok(file) => {
                            println!("Photo path: {}", file.name());
                            let response = proto::TakePhotoResponse {preview_image: vec![1, 2, 3, 4, 5]};
                            Ok(Response::new(response))
                        },
                        Err(e) => {
                            return Err(Status::internal(e.to_string()));
                        }
                    }
                    Err(_) => Err(Status::internal("Timeout"))
                    
                }
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }

    async fn init_camera(&self, request: tonic::Request<proto::InitCameraRequest>) -> Result<tonic::Response<()>, Status> {
        let mut camera_stream = self.context.inner().list_cameras()
                .await
                .map_err(|e| Status::internal(e.to_string()))?;

        let camera_name = request.into_inner().camera;
        let camera_desc = camera_stream.find(|desc| desc.model == camera_name)
            .ok_or_else(|| Status::not_found(format!("Could not find camera with name '{}'", camera_name)))?;
        let camera = self.context.inner().get_camera(&camera_desc)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        
        self.camera.set(camera).await;
        Ok(Response::new(()))
    }

    async fn config_capture_target(&self, _request: tonic::Request<proto::ConfigCaptureTargetRequest>) -> Result<tonic::Response<()>, Status> {
        match self.camera.inner().await {
            Some(camera) => {
                let capture_target = camera.config_key::<RadioWidget>("capturetarget").await.map_err(|e| Status::internal(format!("Failed to get capture target: {}", e)))?;
                capture_target.set_choice("Internal RAM").map_err(|e| Status::internal(format!("Failed to set capture target: {}", e)))?;
                camera.set_config(&capture_target).await.map_err(|e| Status::internal(format!("Failed to set config capture target: {}", e)))?; 
                Ok(Response::new(()))
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }

    async fn config_auto_focus(&self, _request: tonic::Request<proto::ConfigAutoFocusRequest>) -> Result<tonic::Response<()>, Status> {
        match self.camera.inner().await {
            Some(camera) => {
                let cancel_autofocus = camera.config_key::<ToggleWidget>("cancelautofocus").await.map_err(|e| Status::internal(format!("Failed to get cancelautofocus: {}", e)))?;
                cancel_autofocus.set_toggled(false);
                camera.set_config(&cancel_autofocus).await.map_err(|e| Status::internal(format!("Failed to set config cancelautofocus target: {}", e)))?; 
                Ok(Response::new(()))
            },
            None => {
                return Err(Status::failed_precondition("Camera not initialized"));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse().unwrap();
    let photobooth = MyPhotobooth::default();

    let service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    Server::builder()
        .add_service(service)
        .add_service(PhotoBoothServer::new(photobooth))
        .serve(addr)
        .await?;
    Ok(())
}
