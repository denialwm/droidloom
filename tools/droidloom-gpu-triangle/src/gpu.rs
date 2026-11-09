//! Vulkan side: render into exportable LINEAR images and hand out sync_files.
//!
//! The app never uses Vulkan's window-system integration. Each frame ends in
//! a DMA-BUF released to the foreign queue family and a sync_file for its
//! completion, which is exactly what Droidloom's composer hands to Denial.

use std::ffi::CStr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use ash::{ext, khr, vk};

/// `DRM_FORMAT_MOD_LINEAR`: readable by any GPU the compositor renders on.
pub const MODIFIER_LINEAR: u64 = 0;
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// One exported image: the layout Wayland needs plus its Vulkan objects.
pub struct Target {
    /// None for local targets, which never leave this process.
    pub dmabuf: Option<OwnedFd>,
    pub offset: u32,
    pub stride: u32,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    command_buffer: vk::CommandBuffer,
    done: vk::Fence,
    rendered: vk::Semaphore,
    released: vk::Semaphore,
}

pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    memory_fd: khr::external_memory_fd::Device,
    semaphore_fd: khr::external_semaphore_fd::Device,
    queue: vk::Queue,
    queue_family: u32,
    pool: vk::CommandPool,
    memory_types: vk::PhysicalDeviceMemoryProperties,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pub name: String,
    /// Render node of the selected GPU, for the syncobj timelines.
    pub render_node: String,
    pub width: u32,
    pub height: u32,
    pub targets: Vec<Target>,
    /// Draws per frame, each a small triangle in a grid; 1 fills the frame.
    pub draws: u32,
    /// Targets in device-local memory, never exported: headless only.
    local: bool,
}

/// Push constants shared with `triangle.vert`.
#[repr(C)]
struct Frame {
    angle: f32,
    aspect: f32,
    offset: [f32; 2],
    scale: f32,
}

fn spirv(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect()
}

impl Gpu {
    /// Pick the first GPU whose name contains `wanted` and build `count`
    /// targets: exported LINEAR system memory, or device-local when `local`.
    pub fn new(
        wanted: &str,
        width: u32,
        height: u32,
        count: usize,
        local: bool,
    ) -> Result<Self, String> {
        // SAFETY: every call below follows the Vulkan valid-usage rules for
        // the objects created in this function; handles never escape early.
        unsafe { Self::create(wanted, width, height, count, local) }
    }

    unsafe fn create(
        wanted: &str,
        width: u32,
        height: u32,
        count: usize,
        local: bool,
    ) -> Result<Self, String> {
        let entry = unsafe { ash::Entry::load() }.map_err(|error| error.to_string())?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"droidloom-gpu-triangle")
            .api_version(vk::API_VERSION_1_3);
        let instance = unsafe {
            entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None)
        }
        .map_err(|error| format!("create instance: {error}"))?;

        let mut chosen = None;
        for physical in unsafe { instance.enumerate_physical_devices() }
            .map_err(|error| format!("enumerate GPUs: {error}"))?
        {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
            let name = properties
                .properties
                .device_name_as_c_str()
                .unwrap_or(c"?")
                .to_string_lossy()
                .into_owned();
            if name.contains(wanted) && drm.has_render == vk::TRUE {
                chosen = Some((physical, name, drm.render_minor));
                break;
            }
        }
        let (physical, name, render_minor) =
            chosen.ok_or_else(|| format!("no Vulkan GPU with a render node matches {wanted:?}"))?;

        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue")? as u32;

        let extensions: [&CStr; 5] = [
            khr::external_memory_fd::NAME,
            ext::external_memory_dma_buf::NAME,
            ext::image_drm_format_modifier::NAME,
            khr::external_semaphore_fd::NAME,
            ext::queue_family_foreign::NAME,
        ];
        let extension_names = extensions.map(CStr::as_ptr);
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let mut vulkan13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let device = unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .enabled_extension_names(&extension_names)
                    .push_next(&mut vulkan13),
                None,
            )
        }
        .map_err(|error| format!("create device on {name}: {error}"))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let memory_fd = khr::external_memory_fd::Device::new(&instance, &device);
        let semaphore_fd = khr::external_semaphore_fd::Device::new(&instance, &device);

        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }
        .map_err(|error| format!("create command pool: {error}"))?;
        let (layout, pipeline) = unsafe { create_pipeline(&device) }?;

        let memory_types = unsafe { instance.get_physical_device_memory_properties(physical) };
        let mut gpu = Self {
            _entry: entry,
            instance,
            device,
            memory_fd,
            semaphore_fd,
            queue,
            queue_family,
            pool,
            memory_types,
            layout,
            pipeline,
            name,
            render_node: format!("/dev/dri/renderD{render_minor}"),
            width,
            height,
            targets: Vec::with_capacity(count),
            draws: 1,
            local,
        };
        for _ in 0..count {
            let target = unsafe { gpu.create_target(&gpu.memory_types) }?;
            gpu.targets.push(target);
        }
        Ok(gpu)
    }

    /// A device-local optimal image: what an application renders into when
    /// nothing leaves the GPU, the ceiling for forwarding comparisons.
    unsafe fn create_local_image(
        &self,
        memory_types: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<(vk::Image, vk::DeviceMemory), String> {
        let device = &self.device;
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(FORMAT)
                    .extent(vk::Extent3D { width: self.width, height: self.height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(|error| format!("create image: {error}"))?;
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let type_index = (0..memory_types.memory_type_count)
            .find(|&index| {
                requirements.memory_type_bits & (1 << index) != 0
                    && memory_types.memory_types[index as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or("no device-local memory type")?;
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default().allocation_size(requirements.size).memory_type_index(type_index),
                None,
            )
        }
        .map_err(|error| format!("allocate device memory: {error}"))?;
        unsafe { device.bind_image_memory(image, memory, 0) }.map_err(|error| format!("bind image memory: {error}"))?;
        Ok((image, memory))
    }

    unsafe fn create_target(
        &self,
        memory_types: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<Target, String> {
        if self.local {
            let (image, memory) = unsafe { self.create_local_image(memory_types) }?;
            return unsafe { self.finish_target(image, memory, None, 0, 0) };
        }
        let device = &self.device;
        let modifiers = [MODIFIER_LINEAR];
        let mut modifier_list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(FORMAT)
                    .extent(vk::Extent3D { width: self.width, height: self.height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut external)
                    .push_next(&mut modifier_list),
                None,
            )
        }
        .map_err(|error| format!("create LINEAR image: {error}"))?;

        let requirements = unsafe { device.get_image_memory_requirements(image) };
        // System memory only: the compositor may render on another GPU, and
        // a GPU that cannot reach this one's memory rejects the first draw.
        let type_index = (0..memory_types.memory_type_count)
            .find(|&index| {
                let flags = memory_types.memory_types[index as usize].property_flags;
                requirements.memory_type_bits & (1 << index) != 0
                    && flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
                    && !flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or("no system memory type can back an exported image")?;
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(type_index)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            )
        }
        .map_err(|error| format!("allocate exportable memory: {error}"))?;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|error| format!("bind image memory: {error}"))?;

        let raw = unsafe {
            self.memory_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )
        }
        .map_err(|error| format!("export DMA-BUF: {error}"))?;
        // SAFETY: vkGetMemoryFdKHR transfers ownership of a fresh descriptor.
        let dmabuf = unsafe { OwnedFd::from_raw_fd(raw) };
        let plane = unsafe {
            device.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            )
        };
        let offset = u32::try_from(plane.offset).map_err(|_| "plane offset too large")?;
        let stride = u32::try_from(plane.row_pitch).map_err(|_| "row pitch too large")?;
        unsafe { self.finish_target(image, memory, Some(dmabuf), offset, stride) }
    }

    /// View, command buffer, fence and semaphores for one target.
    unsafe fn finish_target(
        &self,
        image: vk::Image,
        memory: vk::DeviceMemory,
        dmabuf: Option<OwnedFd>,
        offset: u32,
        stride: u32,
    ) -> Result<Target, String> {
        let device = &self.device;

        let view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(FORMAT)
                    .subresource_range(color_range()),
                None,
            )
        }
        .map_err(|error| format!("create image view: {error}"))?;
        let command_buffer = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(|error| format!("allocate command buffer: {error}"))?[0];
        let done = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        }
        .map_err(|error| format!("create fence: {error}"))?;
        let mut exportable = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let rendered = unsafe {
            device.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut exportable),
                None,
            )
        }
        .map_err(|error| format!("create exportable semaphore: {error}"))?;
        let released = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
            .map_err(|error| format!("create release semaphore: {error}"))?;

        Ok(Target {
            dmabuf,
            offset,
            stride,
            image,
            view,
            memory,
            command_buffer,
            done,
            rendered,
            released,
        })
    }

    /// Render target `index` at `angle`. `release` is the compositor's fence
    /// for the previous use of this image; the GPU waits for it, never the CPU.
    /// Returns the sync_file that signals when the frame is complete.
    pub fn render(&self, index: usize, angle: f32, release: Option<OwnedFd>) -> Result<OwnedFd, String> {
        // SAFETY: the fence wait below guarantees the command buffer and
        // semaphores of this target are idle before they are reused.
        unsafe { self.render_unchecked(index, angle, release) }
    }

    unsafe fn render_unchecked(
        &self,
        index: usize,
        angle: f32,
        release: Option<OwnedFd>,
    ) -> Result<OwnedFd, String> {
        let device = &self.device;
        let target = &self.targets[index];
        unsafe { device.wait_for_fences(&[target.done], true, u64::MAX) }
            .map_err(|error| format!("wait for target: {error}"))?;
        unsafe { device.reset_fences(&[target.done]) }.map_err(|error| error.to_string())?;

        let waits_on_release = if let Some(fence) = release {
            use std::os::fd::IntoRawFd;
            let raw: RawFd = fence.into_raw_fd();
            // A temporary SYNC_FD import takes ownership of the descriptor.
            unsafe {
                self.semaphore_fd.import_semaphore_fd(
                    &vk::ImportSemaphoreFdInfoKHR::default()
                        .semaphore(target.released)
                        .flags(vk::SemaphoreImportFlags::TEMPORARY)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
                        .fd(raw),
                )
            }
            .map_err(|error| format!("import release fence: {error}"))?;
            true
        } else {
            false
        };

        let commands = target.command_buffer;
        unsafe {
            device.begin_command_buffer(
                commands,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .map_err(|error| error.to_string())?;
        let acquire = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(if self.local { vk::QUEUE_FAMILY_IGNORED } else { vk::QUEUE_FAMILY_FOREIGN_EXT })
            .dst_queue_family_index(if self.local { vk::QUEUE_FAMILY_IGNORED } else { self.queue_family })
            .image(target.image)
            .subresource_range(color_range())];
        unsafe {
            device.cmd_pipeline_barrier2(
                commands,
                &vk::DependencyInfo::default().image_memory_barriers(&acquire),
            );
        }
        let attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(target.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.04, 0.035, 0.06, 1.0] },
            })];
        let area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D { width: self.width, height: self.height },
        };
        let aspect = self.width as f32 / self.height as f32;
        let side = (self.draws as f32).sqrt().ceil() as u32;
        let cell = 2.0 / side as f32;
        unsafe {
            device.cmd_begin_rendering(
                commands,
                &vk::RenderingInfo::default()
                    .render_area(area)
                    .layer_count(1)
                    .color_attachments(&attachment),
            );
            device.cmd_bind_pipeline(commands, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            device.cmd_set_viewport(
                commands,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: self.width as f32,
                    height: self.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            device.cmd_set_scissor(commands, 0, &[area]);
            for i in 0..self.draws {
                let (column, row) = (i % side, i / side);
                let frame = Frame {
                    angle: angle + i as f32 * 0.01,
                    aspect,
                    offset: [
                        if side == 1 { 0.0 } else { -1.0 + cell * (column as f32 + 0.5) },
                        if side == 1 { 0.0 } else { -1.0 + cell * (row as f32 + 0.5) },
                    ],
                    scale: 1.0 / side as f32,
                };
                device.cmd_push_constants(
                    commands,
                    self.layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    std::slice::from_raw_parts((&raw const frame).cast::<u8>(), size_of::<Frame>()),
                );
                device.cmd_draw(commands, 3, 1, 0, 0);
            }
            device.cmd_end_rendering(commands);
        }
        let release_barrier = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::NONE)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(if self.local { vk::QUEUE_FAMILY_IGNORED } else { self.queue_family })
            .dst_queue_family_index(if self.local { vk::QUEUE_FAMILY_IGNORED } else { vk::QUEUE_FAMILY_FOREIGN_EXT })
            .image(target.image)
            .subresource_range(color_range())];
        unsafe {
            device.cmd_pipeline_barrier2(
                commands,
                &vk::DependencyInfo::default().image_memory_barriers(&release_barrier),
            );
            device.end_command_buffer(commands)
        }
        .map_err(|error| error.to_string())?;

        let wait = [vk::SemaphoreSubmitInfo::default()
            .semaphore(target.released)
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
        let buffers = [vk::CommandBufferSubmitInfo::default().command_buffer(commands)];
        let signal = [vk::SemaphoreSubmitInfo::default()
            .semaphore(target.rendered)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let mut submit = vk::SubmitInfo2::default()
            .command_buffer_infos(&buffers)
            .signal_semaphore_infos(&signal);
        if waits_on_release {
            submit = submit.wait_semaphore_infos(&wait);
        }
        unsafe { device.queue_submit2(self.queue, &[submit], target.done) }
            .map_err(|error| format!("submit: {error}"))?;

        let raw = unsafe {
            self.semaphore_fd.get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(target.rendered)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
            )
        }
        .map_err(|error| format!("export frame fence: {error}"))?;
        // SAFETY: a SYNC_FD export returns a fresh descriptor we now own.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

impl Gpu {
    /// `check_memory_type` on every mappable memory type, system memory and
    /// VRAM alike.
    pub fn check_memory(&self) -> Result<String, String> {
        let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let lines = (0..self.memory_types.memory_type_count)
            .filter(|&index| self.memory_types.memory_types[index as usize].property_flags.contains(wanted))
            .map(|index| self.check_memory_type(index))
            .collect::<Result<Vec<_>, _>>()?;
        if lines.is_empty() {
            return Err("no host-visible coherent memory".into());
        }
        Ok(lines.join("\n"))
    }

    /// Mapped memory both ways: the CPU writes a pattern, the GPU copies it
    /// and fills a tail, the CPU reads the result back.
    fn check_memory_type(&self, type_index: u32) -> Result<String, String> {
        const SIZE: u64 = 1 << 20;
        const TAIL: u64 = 4096;
        // SAFETY: every object is created, used and destroyed here, and the
        // fence wait orders the GPU's writes before the CPU reads them.
        unsafe {
            let device = &self.device;
            let started = std::time::Instant::now();
            let mut buffers = Vec::new();
            for _ in 0..2 {
                let buffer = device
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(SIZE)
                            .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST),
                        None,
                    )
                    .map_err(|error| format!("create buffer: {error}"))?;
                let requirements = device.get_buffer_memory_requirements(buffer);
                if requirements.memory_type_bits & (1 << type_index) == 0 {
                    return Err(format!("buffers cannot use memory type {type_index}"));
                }
                let memory = device
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default().allocation_size(requirements.size).memory_type_index(type_index),
                        None,
                    )
                    .map_err(|error| format!("allocate memory: {error}"))?;
                device.bind_buffer_memory(buffer, memory, 0).map_err(|error| format!("bind buffer: {error}"))?;
                let data = device
                    .map_memory(memory, 0, SIZE, vk::MemoryMapFlags::empty())
                    .map_err(|error| format!("map memory: {error}"))?
                    .cast::<u32>();
                buffers.push((buffer, memory, std::slice::from_raw_parts_mut(data, (SIZE / 4) as usize)));
            }
            let setup = started.elapsed();
            for (i, word) in buffers[0].2.iter_mut().enumerate() {
                *word = (i as u32).wrapping_mul(2_654_435_761);
            }
            buffers[1].2.fill(0);
            let commands = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(|error| format!("allocate command buffer: {error}"))?[0];
            device
                .begin_command_buffer(commands, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))
                .map_err(|error| error.to_string())?;
            device.cmd_copy_buffer(commands, buffers[0].0, buffers[1].0, &[vk::BufferCopy { src_offset: 0, dst_offset: 0, size: SIZE - TAIL }]);
            let between = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR | vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE | vk::AccessFlags2::HOST_READ)];
            device.cmd_pipeline_barrier2(commands, &vk::DependencyInfo::default().memory_barriers(&between));
            device.cmd_fill_buffer(commands, buffers[1].0, SIZE - TAIL, TAIL, 0xdead_beef);
            let to_host = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)];
            device.cmd_pipeline_barrier2(commands, &vk::DependencyInfo::default().memory_barriers(&to_host));
            device.end_command_buffer(commands).map_err(|error| error.to_string())?;
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(|error| error.to_string())?;
            let command_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(commands)];
            device
                .queue_submit2(self.queue, &[vk::SubmitInfo2::default().command_buffer_infos(&command_infos)], fence)
                .map_err(|error| format!("submit: {error}"))?;
            let submitted = std::time::Instant::now();
            device.wait_for_fences(&[fence], true, u64::MAX).map_err(|error| format!("wait: {error}"))?;
            let gpu = submitted.elapsed();
            let tail_start = ((SIZE - TAIL) / 4) as usize;
            let wrong = buffers[1]
                .2
                .iter()
                .enumerate()
                .filter(|&(i, &word)| word != if i < tail_start { (i as u32).wrapping_mul(2_654_435_761) } else { 0xdead_beef })
                .count();
            device.destroy_fence(fence, None);
            device.free_command_buffers(self.pool, &[commands]);
            for (buffer, memory, _) in buffers {
                device.unmap_memory(memory);
                device.destroy_buffer(buffer, None);
                device.free_memory(memory, None);
            }
            if wrong != 0 {
                return Err(format!("memory check: {wrong} of {} words wrong", SIZE / 4));
            }
            let local = self.memory_types.memory_types[type_index as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL);
            Ok(format!(
                "memory check, type {type_index} ({}): 1 MiB written by the CPU, copied and filled by the GPU, read back exact (setup {:.0} µs, GPU {:.0} µs)",
                if local { "VRAM" } else { "system memory" },
                setup.as_secs_f64() * 1e6,
                gpu.as_secs_f64() * 1e6
            ))
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: the device is idle after the wait; objects die in reverse order.
        unsafe {
            let _ = self.device.device_wait_idle();
            for target in self.targets.drain(..) {
                self.device.destroy_semaphore(target.released, None);
                self.device.destroy_semaphore(target.rendered, None);
                self.device.destroy_fence(target.done, None);
                self.device.destroy_image_view(target.view, None);
                self.device.destroy_image(target.image, None);
                self.device.free_memory(target.memory, None);
            }
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

unsafe fn create_pipeline(device: &ash::Device) -> Result<(vk::PipelineLayout, vk::Pipeline), String> {
    let vertex_code = spirv(include_bytes!("../shaders/triangle.vert.spv"));
    let fragment_code = spirv(include_bytes!("../shaders/triangle.frag.spv"));
    let vertex = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vertex_code), None)
    }
    .map_err(|error| format!("create vertex shader: {error}"))?;
    let fragment = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&fragment_code), None)
    }
    .map_err(|error| format!("create fragment shader: {error}"))?;

    let push = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::VERTEX)
        .size(size_of::<Frame>() as u32)];
    let layout = unsafe {
        device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push),
            None,
        )
    }
    .map_err(|error| format!("create pipeline layout: {error}"))?;

    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment)
            .name(c"main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let formats = [FORMAT];
    let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .push_next(&mut rendering);
    let pipeline = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
    }
    .map_err(|(_, error)| format!("create pipeline: {error}"))?[0];
    unsafe {
        device.destroy_shader_module(vertex, None);
        device.destroy_shader_module(fragment, None);
    }
    Ok((layout, pipeline))
}
