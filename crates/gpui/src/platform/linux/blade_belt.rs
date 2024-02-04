use blade_graphics as gpu;
use std::mem;

struct ReusableBuffer {
    raw: gpu::Buffer,
    size: u64,
}

pub struct BladeBeltDescriptor {
    pub memory: gpu::Memory,
    pub min_chunk_size: u64,
}

/// A belt of buffers, used by the BladeAtlas to cheaply
/// find staging space for uploads.
pub struct BladeBelt {
    desc: BladeBeltDescriptor,
    buffers: Vec<(ReusableBuffer, gpu::SyncPoint)>,
    active: Vec<(ReusableBuffer, u64)>,
}

impl BladeBelt {
    pub fn new(desc: BladeBeltDescriptor) -> Self {
        Self {
            desc,
            buffers: Vec::new(),
            active: Vec::new(),
        }
    }

    pub fn destroy(&mut self, gpu: &gpu::Context) {
        for (buffer, _) in self.buffers.drain(..) {
            gpu.destroy_buffer(buffer.raw);
        }
        for (buffer, _) in self.active.drain(..) {
            gpu.destroy_buffer(buffer.raw);
        }
    }

    pub fn alloc(&mut self, size: u64, gpu: &gpu::Context) -> gpu::BufferPiece {
        for &mut (ref rb, ref mut offset) in self.active.iter_mut() {
            if *offset + size <= rb.size {
                let piece = rb.raw.at(*offset);
                *offset += size;
                return piece;
            }
        }

        let index_maybe = self
            .buffers
            .iter()
            .position(|&(ref rb, ref sp)| size <= rb.size && gpu.wait_for(sp, 0));
        if let Some(index) = index_maybe {
            let (rb, _) = self.buffers.remove(index);
            let piece = rb.raw.into();
            self.active.push((rb, size));
            return piece;
        }

        let chunk_index = self.buffers.len() + self.active.len();
        let chunk_size = size.max(self.desc.min_chunk_size);
        let chunk = gpu.create_buffer(gpu::BufferDesc {
            name: &format!("chunk-{}", chunk_index),
            size: chunk_size,
            memory: self.desc.memory,
        });
        let rb = ReusableBuffer {
            raw: chunk,
            size: chunk_size,
        };
        self.active.push((rb, size));
        chunk.into()
    }

    //Note: assuming T: bytemuck::Zeroable
    pub fn alloc_data<T>(&mut self, data: &[T], gpu: &gpu::Context) -> gpu::BufferPiece {
        assert!(!data.is_empty());
        let alignment = mem::align_of::<T>() as u64;
        let total_bytes = data.len() * mem::size_of::<T>();
        let mut bp = self.alloc(alignment + (total_bytes - 1) as u64, gpu);
        let rem = bp.offset % alignment;
        if rem != 0 {
            bp.offset += alignment - rem;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, bp.data(), total_bytes);
        }
        bp
    }

    pub fn flush(&mut self, sp: &gpu::SyncPoint) {
        self.buffers
            .extend(self.active.drain(..).map(|(rb, _)| (rb, sp.clone())));
    }
}