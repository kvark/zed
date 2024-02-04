use super::{BladeBelt, BladeBeltDescriptor};
use crate::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, DevicePixels, PlatformAtlas,
    Point, Size,
};
use anyhow::Result;
use blade_graphics as gpu;
use collections::FxHashMap;
use etagere::BucketedAtlasAllocator;
use parking_lot::Mutex;
use std::{borrow::Cow, sync::Arc};

pub(crate) const PATH_TEXTURE_FORMAT: gpu::TextureFormat = gpu::TextureFormat::R16Float;

pub(crate) struct BladeAtlas(Mutex<BladeAtlasState>);

struct PendingUpload {
    id: AtlasTextureId,
    bounds: Bounds<DevicePixels>,
    data: gpu::BufferPiece,
}

struct BladeAtlasState {
    gpu: Arc<gpu::Context>,
    upload_belt: BladeBelt,
    monochrome_textures: Vec<BladeAtlasTexture>,
    polychrome_textures: Vec<BladeAtlasTexture>,
    path_textures: Vec<BladeAtlasTexture>,
    tiles_by_key: FxHashMap<AtlasKey, AtlasTile>,
    uploads: Vec<PendingUpload>,
}

impl BladeAtlasState {
    fn destroy(&mut self) {
        for texture in self.monochrome_textures.drain(..) {
            self.gpu.destroy_texture(texture.raw);
        }
        for texture in self.polychrome_textures.drain(..) {
            self.gpu.destroy_texture(texture.raw);
        }
        for texture in self.path_textures.drain(..) {
            self.gpu.destroy_texture(texture.raw);
            self.gpu.destroy_texture_view(texture.raw_view.unwrap());
        }
        self.upload_belt.destroy(&self.gpu);
    }
}

pub struct BladeTextureInfo {
    pub size: gpu::Extent,
    pub raw_view: Option<gpu::TextureView>,
}

impl BladeAtlas {
    pub(crate) fn new(gpu: &Arc<gpu::Context>) -> Self {
        BladeAtlas(Mutex::new(BladeAtlasState {
            gpu: Arc::clone(gpu),
            upload_belt: BladeBelt::new(BladeBeltDescriptor {
                memory: gpu::Memory::Upload,
                min_chunk_size: 0x10000,
            }),
            monochrome_textures: Default::default(),
            polychrome_textures: Default::default(),
            path_textures: Default::default(),
            tiles_by_key: Default::default(),
            uploads: Vec::new(),
        }))
    }

    pub(crate) fn destroy(&self) {
        self.0.lock().destroy();
    }

    pub(crate) fn clear_textures(&self, texture_kind: AtlasTextureKind) {
        let mut lock = self.0.lock();
        let textures = match texture_kind {
            AtlasTextureKind::Monochrome => &mut lock.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut lock.polychrome_textures,
            AtlasTextureKind::Path => &mut lock.path_textures,
        };
        for texture in textures {
            texture.clear();
        }
    }

    pub fn allocate(&self, size: Size<DevicePixels>, texture_kind: AtlasTextureKind) -> AtlasTile {
        let mut lock = self.0.lock();
        lock.allocate(size, texture_kind)
    }

    pub fn before_frame(&self, gpu_encoder: &mut gpu::CommandEncoder) {
        let mut lock = self.0.lock();
        lock.flush(gpu_encoder.transfer());
    }

    pub fn after_frame(&self, sync_point: &gpu::SyncPoint) {
        let mut lock = self.0.lock();
        lock.upload_belt.flush(sync_point);
    }

    pub fn get_texture_info(&self, id: AtlasTextureId) -> BladeTextureInfo {
        let lock = self.0.lock();
        let textures = match id.kind {
            crate::AtlasTextureKind::Monochrome => &lock.monochrome_textures,
            crate::AtlasTextureKind::Polychrome => &lock.polychrome_textures,
            crate::AtlasTextureKind::Path => &lock.path_textures,
        };
        let texture = &textures[id.index as usize];
        let size = texture.allocator.size();
        BladeTextureInfo {
            size: gpu::Extent {
                width: size.width as u32,
                height: size.height as u32,
                depth: 1,
            },
            raw_view: texture.raw_view,
        }
    }
}

impl PlatformAtlas for BladeAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<(Size<DevicePixels>, Cow<'a, [u8]>)>,
    ) -> Result<AtlasTile> {
        let mut lock = self.0.lock();
        if let Some(tile) = lock.tiles_by_key.get(key) {
            Ok(tile.clone())
        } else {
            let (size, bytes) = build()?;
            let tile = lock.allocate(size, key.texture_kind());
            lock.upload_texture(tile.texture_id, tile.bounds, &bytes);
            lock.tiles_by_key.insert(key.clone(), tile.clone());
            Ok(tile)
        }
    }
}

impl BladeAtlasState {
    fn allocate(&mut self, size: Size<DevicePixels>, texture_kind: AtlasTextureKind) -> AtlasTile {
        let textures = match texture_kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
            AtlasTextureKind::Path => &mut self.path_textures,
        };
        textures
            .iter_mut()
            .rev()
            .find_map(|texture| texture.allocate(size))
            .unwrap_or_else(|| {
                let texture = self.push_texture(size, texture_kind);
                texture.allocate(size).unwrap()
            })
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> &mut BladeAtlasTexture {
        const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(1024),
            height: DevicePixels(1024),
        };

        let size = min_size.max(&DEFAULT_ATLAS_SIZE);
        let format;
        let usage;
        match kind {
            AtlasTextureKind::Monochrome => {
                format = gpu::TextureFormat::R8Unorm;
                usage = gpu::TextureUsage::COPY | gpu::TextureUsage::RESOURCE;
            }
            AtlasTextureKind::Polychrome => {
                format = gpu::TextureFormat::Bgra8Unorm;
                usage = gpu::TextureUsage::COPY | gpu::TextureUsage::RESOURCE;
            }
            AtlasTextureKind::Path => {
                format = PATH_TEXTURE_FORMAT;
                usage = gpu::TextureUsage::COPY
                    | gpu::TextureUsage::RESOURCE
                    | gpu::TextureUsage::TARGET;
            }
        }

        let raw = self.gpu.create_texture(gpu::TextureDesc {
            name: "atlas",
            format,
            size: gpu::Extent {
                width: size.width.into(),
                height: size.height.into(),
                depth: 1,
            },
            array_layer_count: 1,
            mip_level_count: 1,
            dimension: gpu::TextureDimension::D2,
            usage,
        });
        let raw_view = if usage.contains(gpu::TextureUsage::TARGET) {
            Some(self.gpu.create_texture_view(gpu::TextureViewDesc {
                name: "",
                texture: raw,
                format,
                dimension: gpu::ViewDimension::D2,
                subresources: &Default::default(),
            }))
        } else {
            None
        };

        let textures = match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
            AtlasTextureKind::Path => &mut self.path_textures,
        };
        let atlas_texture = BladeAtlasTexture {
            id: AtlasTextureId {
                index: textures.len() as u32,
                kind,
            },
            allocator: etagere::BucketedAtlasAllocator::new(size.into()),
            format,
            raw,
            raw_view,
        };
        textures.push(atlas_texture);
        textures.last_mut().unwrap()
    }

    fn upload_texture(&mut self, id: AtlasTextureId, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        let data = self.upload_belt.alloc_data(bytes, &self.gpu);
        self.uploads.push(PendingUpload { id, bounds, data });
    }

    fn flush(&mut self, mut transfers: gpu::TransferCommandEncoder) {
        for upload in self.uploads.drain(..) {
            let textures = match upload.id.kind {
                crate::AtlasTextureKind::Monochrome => &self.monochrome_textures,
                crate::AtlasTextureKind::Polychrome => &self.polychrome_textures,
                crate::AtlasTextureKind::Path => &self.path_textures,
            };
            let texture = &textures[upload.id.index as usize];

            transfers.copy_buffer_to_texture(
                upload.data,
                upload.bounds.size.width.to_bytes(texture.bytes_per_pixel()),
                gpu::TexturePiece {
                    texture: texture.raw,
                    mip_level: 0,
                    array_layer: 0,
                    origin: [
                        upload.bounds.origin.x.into(),
                        upload.bounds.origin.y.into(),
                        0,
                    ],
                },
                gpu::Extent {
                    width: upload.bounds.size.width.into(),
                    height: upload.bounds.size.height.into(),
                    depth: 1,
                },
            );
        }
    }
}

struct BladeAtlasTexture {
    id: AtlasTextureId,
    allocator: BucketedAtlasAllocator,
    raw: gpu::Texture,
    raw_view: Option<gpu::TextureView>,
    format: gpu::TextureFormat,
}

impl BladeAtlasTexture {
    fn clear(&mut self) {
        self.allocator.clear();
    }

    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.allocate(size.into())?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            padding: 0,
            bounds: Bounds {
                origin: allocation.rectangle.min.into(),
                size,
            },
        };
        Some(tile)
    }

    fn bytes_per_pixel(&self) -> u8 {
        self.format.block_info().size
    }
}

impl From<Size<DevicePixels>> for etagere::Size {
    fn from(size: Size<DevicePixels>) -> Self {
        etagere::Size::new(size.width.into(), size.height.into())
    }
}

impl From<etagere::Point> for Point<DevicePixels> {
    fn from(value: etagere::Point) -> Self {
        Point {
            x: DevicePixels::from(value.x),
            y: DevicePixels::from(value.y),
        }
    }
}

impl From<etagere::Size> for Size<DevicePixels> {
    fn from(size: etagere::Size) -> Self {
        Size {
            width: DevicePixels::from(size.width),
            height: DevicePixels::from(size.height),
        }
    }
}

impl From<etagere::Rectangle> for Bounds<DevicePixels> {
    fn from(rectangle: etagere::Rectangle) -> Self {
        Bounds {
            origin: rectangle.min.into(),
            size: rectangle.size().into(),
        }
    }
}