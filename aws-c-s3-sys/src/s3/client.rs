use crate::auth::credentials::CredentialsProvider;
use crate::auth::signing_config::SigningConfig;
use crate::common::allocator::Allocator;
use crate::generated::*;
use crate::io::channel_bootstrap::ClientBootstrap;
use crate::StringExt;
use std::ptr::NonNull;
use std::sync::Arc;

pub struct Client<'user> {
    // TODO: make only visible to this crate
    pub inner: NonNull<aws_s3_client>,

    // We need to hold onto the signing config for as long as the client exists
    // The signing config itself references some bytes owned by the user (e.g., region string).
    signing_config: SigningConfig<'user>,
}

pub struct ClientConfig<'a, 'user> {
    pub max_active_connections_override: Option<u32>,
    pub throughput_target_gbps: f64,
    pub part_size: usize,
    pub client_bootstrap: &'a mut ClientBootstrap,
    pub signing_config: &'a SigningConfig<'user>,
}

impl<'user> Client<'user> {
    pub fn new(allocator: &mut Allocator, config: &ClientConfig<'_, 'user>) -> Option<Self> {
        let signing_config = config.signing_config.clone();

        let inner_config = aws_s3_client_config {
            max_active_connections_override: config.max_active_connections_override.unwrap_or(0),
            throughput_target_gbps: config.throughput_target_gbps,
            client_bootstrap: config.client_bootstrap.inner.as_ptr(),
            part_size: config.part_size,
            signing_config: unsafe { std::mem::transmute((&*signing_config.inner) as *const _) },
            ..Default::default()
        };

        let inner = unsafe { aws_s3_client_new(allocator.inner.as_ptr(), &inner_config) };

        Some(Self {
            inner: NonNull::new(inner)?,
            signing_config,
        })
    }
}

impl<'user> Clone for Client<'user> {
    fn clone(&self) -> Self {
        unsafe {
            aws_s3_client_acquire(self.inner.as_ptr());
        }

        Self {
            inner: self.inner,
            signing_config: self.signing_config.clone(),
        }
    }
}

impl<'user> Drop for Client<'user> {
    fn drop(&mut self) {
        unsafe {
            aws_s3_client_release(self.inner.as_ptr());
        }
    }
}

pub fn init_default_signing_config<'user>(
    region: &'user str,
    credentials_provider: &mut CredentialsProvider,
) -> SigningConfig<'user> {
    let region_cursor = unsafe { region.as_aws_byte_cursor() };

    let mut signing_config = Default::default();

    unsafe {
        aws_s3_init_default_signing_config(&mut signing_config, region_cursor, credentials_provider.inner.as_ptr());
    }

    signing_config.flags.set_use_double_uri_encode(false as u32);

    SigningConfig {
        inner: Arc::new(signing_config),
        phantom: Default::default(),
    }
}
