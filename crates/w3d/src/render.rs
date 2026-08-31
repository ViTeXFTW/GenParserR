use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use glam::{Mat4, Vec2, Vec3, Vec4};
use image::{DynamicImage, ImageEncoder, ImageFormat, Rgba, RgbaImage};

use crate::parse::{Hierarchy, Mesh, SubObject};
use crate::{W3dError, W3dFile};

pub const WIDTH: u32 = 160;
pub const HEIGHT: u32 = 120;
const HIDDEN: u32 = 0x0000_1000;
const TWO_SIDED: u32 = 0x0000_2000;
const COLLISION_MASK: u32 = 0x0000_0ff0;
const MAX_RENDER_TRIANGLES: usize = 10_000;
const MAX_RENDER_TEXTURES: usize = 32;
const MAX_RENDER_TEXTURE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_RASTER_SAMPLES: u64 = WIDTH as u64 * HEIGHT as u64 * 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedThumbnail {
    pub png: Vec<u8>,
    pub missing_textures: Vec<String>,
}

#[derive(Clone)]
struct Vertex {
    position: Vec3,
    normal: Vec3,
    uv: Vec2,
    color: Vec4,
}

struct DrawTriangle {
    vertices: [Vertex; 3],
    texture: String,
    two_sided: bool,
}

struct Texture {
    image: RgbaImage,
}

pub(crate) fn thumbnail(
    file: &W3dFile,
    model: &str,
    zoom: f32,
    mut load_texture: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Result<RenderedThumbnail, W3dError> {
    let hierarchy = selected_hierarchy(file, model);
    let bones = hierarchy.map(rest_pose).unwrap_or_default();
    let selected = selected_meshes(file, model);
    let triangle_count = selected
        .iter()
        .try_fold(0usize, |total, (mesh, _)| {
            total.checked_add(mesh.triangles.len())
        })
        .filter(|total| *total <= MAX_RENDER_TRIANGLES)
        .ok_or_else(|| W3dError::new("model triangle budget exceeded"))?;
    let mut triangles = Vec::with_capacity(triangle_count);
    for (mesh, fallback_bone) in selected {
        append_mesh_triangles(mesh, fallback_bone, &bones, &mut triangles);
    }
    if triangles.is_empty() {
        return Err(W3dError::new("model has no visible triangles"));
    }

    let mut textures = HashMap::new();
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    let mut texture_pixels = 0u64;
    for name in triangles
        .iter()
        .map(|triangle| triangle.texture.as_str())
        .filter(|name| !name.is_empty())
    {
        let key = name.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        if seen.len() == MAX_RENDER_TEXTURES {
            return Err(W3dError::new("model texture count budget exceeded"));
        }
        seen.insert(key.clone());
        let texture = load_texture(name)
            .and_then(|bytes| decode_texture(&bytes).ok())
            .unwrap_or_else(|| {
                missing.push(name.to_string());
                missing_texture()
            });
        texture_pixels = texture_pixels
            .checked_add(u64::from(texture.image.width()) * u64::from(texture.image.height()))
            .filter(|pixels| *pixels <= MAX_RENDER_TEXTURE_PIXELS)
            .ok_or_else(|| W3dError::new("model texture memory budget exceeded"))?;
        textures.insert(key, texture);
    }

    let mut pixels = checkerboard();
    let mut depth = vec![f32::INFINITY; (WIDTH * HEIGHT) as usize];
    rasterize(
        &triangles,
        &textures,
        zoom.clamp(0.25, 4.0),
        &mut pixels,
        &mut depth,
    )?;
    let pixels = DynamicImage::ImageRgba8(pixels).into_rgb8();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(pixels.as_raw(), WIDTH, HEIGHT, image::ColorType::Rgb8)
        .map_err(|error| W3dError::new(format!("could not encode preview: {error}")))?;
    missing.sort_by_key(|name| name.to_ascii_lowercase());
    missing.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    Ok(RenderedThumbnail {
        png,
        missing_textures: missing,
    })
}

fn selected_hierarchy<'a>(file: &'a W3dFile, model: &str) -> Option<&'a Hierarchy> {
    let requested = file
        .hlods
        .iter()
        .find(|hlod| hlod.name.eq_ignore_ascii_case(model))
        .or_else(|| file.hlods.first())
        .map(|hlod| hlod.hierarchy.as_str());
    requested
        .and_then(|name| {
            file.hierarchies
                .iter()
                .find(|hierarchy| hierarchy.name.eq_ignore_ascii_case(name))
        })
        .or_else(|| file.hierarchies.first())
}

fn selected_meshes<'a>(file: &'a W3dFile, model: &str) -> Vec<(&'a Mesh, usize)> {
    if let Some(hlod) = file
        .hlods
        .iter()
        .find(|hlod| hlod.name.eq_ignore_ascii_case(model))
    {
        let selected = meshes_for_hlod(file, hlod);
        if !selected.is_empty() {
            return selected;
        }
    }

    let matching: Vec<_> = file
        .meshes
        .iter()
        .filter(|mesh| visible(mesh) && mesh_matches(mesh, model))
        .map(|mesh| (mesh, 0))
        .collect();
    if !matching.is_empty() {
        return matching;
    }
    file.hlods
        .first()
        .map(|hlod| meshes_for_hlod(file, hlod))
        .filter(|selected| !selected.is_empty())
        .unwrap_or_else(|| {
            file.meshes
                .iter()
                .filter(|mesh| visible(mesh))
                .map(|mesh| (mesh, 0))
                .collect()
        })
}

fn meshes_for_hlod<'a>(file: &'a W3dFile, hlod: &'a crate::parse::Hlod) -> Vec<(&'a Mesh, usize)> {
    let mut subobjects: Vec<&SubObject> = hlod.aggregates.iter().collect();
    if let Some(lod) = hlod
        .lods
        .iter()
        .max_by(|left, right| left.max_screen_size.total_cmp(&right.max_screen_size))
    {
        subobjects.extend(&lod.subobjects);
    }
    subobjects
        .into_iter()
        .filter_map(|subobject| {
            find_mesh(&file.meshes, &subobject.name).map(|mesh| (mesh, subobject.bone))
        })
        .filter(|(mesh, _)| visible(mesh))
        .collect()
}

fn visible(mesh: &Mesh) -> bool {
    mesh.attributes & (HIDDEN | COLLISION_MASK) == 0
}

fn mesh_matches(mesh: &Mesh, name: &str) -> bool {
    mesh.name.eq_ignore_ascii_case(name)
        || mesh.container.eq_ignore_ascii_case(name)
        || format!("{}.{}", mesh.container, mesh.name).eq_ignore_ascii_case(name)
}

fn find_mesh<'a>(meshes: &'a [Mesh], name: &str) -> Option<&'a Mesh> {
    let short = name
        .rsplit_once('.')
        .map(|(_, short)| short)
        .unwrap_or(name);
    meshes.iter().find(|mesh| {
        mesh_matches(mesh, name)
            || mesh.name.eq_ignore_ascii_case(short)
            || format!("{}.{}", mesh.container, mesh.name).eq_ignore_ascii_case(name)
    })
}

fn rest_pose(hierarchy: &Hierarchy) -> Vec<Mat4> {
    let mut bones = Vec::with_capacity(hierarchy.pivots.len());
    for pivot in &hierarchy.pivots {
        let local = Mat4::from_rotation_translation(pivot.rotation, pivot.translation);
        let world = pivot
            .parent
            .and_then(|parent| bones.get(parent))
            .copied()
            .unwrap_or(Mat4::IDENTITY)
            * local;
        bones.push(world);
    }
    bones
}

fn append_mesh_triangles(
    mesh: &Mesh,
    fallback_bone: usize,
    bones: &[Mat4],
    out: &mut Vec<DrawTriangle>,
) {
    let uv_source = if mesh.uvs.is_empty() {
        &mesh.pass.uvs
    } else {
        &mesh.uvs
    };
    for (triangle_index, triangle) in mesh.triangles.iter().enumerate() {
        let texture_id = match mesh.pass.texture_ids.as_slice() {
            [only] => *only as usize,
            many if triangle_index < many.len() => many[triangle_index] as usize,
            _ => 0,
        };
        let texture = mesh.textures.get(texture_id).cloned().unwrap_or_default();
        let mut vertices = Vec::with_capacity(3);
        for corner in 0..3 {
            let index = triangle.indices[corner] as usize;
            let Some(position) = mesh.vertices.get(index).copied() else {
                vertices.clear();
                break;
            };
            let normal = mesh.normals.get(index).copied().unwrap_or(Vec3::Z);
            let uv_index = mesh
                .pass
                .per_face_uv_ids
                .get(triangle_index * 3 + corner)
                .copied()
                .map(|value| value as usize)
                .unwrap_or(index);
            let uv = uv_source.get(uv_index).copied().unwrap_or(Vec2::ZERO);
            let color = mesh
                .colors
                .get(index)
                .or_else(|| mesh.pass.colors.get(index))
                .copied()
                .unwrap_or(mesh.material_diffuse);
            let transform = mesh
                .influences
                .get(index)
                .map(|bone| *bone as usize)
                .or(Some(fallback_bone))
                .and_then(|bone| bones.get(bone))
                .copied()
                .unwrap_or(Mat4::IDENTITY);
            vertices.push(Vertex {
                position: transform.transform_point3(position),
                normal: transform.transform_vector3(normal).normalize_or_zero(),
                uv,
                color: Vec4::new(
                    color[0] as f32 / 255.0,
                    color[1] as f32 / 255.0,
                    color[2] as f32 / 255.0,
                    color[3] as f32 / 255.0,
                ),
            });
        }
        if let Ok(vertices) = <Vec<Vertex> as TryInto<[Vertex; 3]>>::try_into(vertices) {
            out.push(DrawTriangle {
                vertices,
                texture,
                two_sided: mesh.attributes & TWO_SIDED != 0,
            });
        }
    }
}

fn decode_texture(bytes: &[u8]) -> Result<Texture, W3dError> {
    let format = image::guess_format(bytes).unwrap_or(ImageFormat::Tga);
    let mut reader = image::io::Reader::with_format(Cursor::new(bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|error| W3dError::new(format!("unsupported texture: {error}")))?;
    checked_texture(decoded)
}

fn checked_texture(image: DynamicImage) -> Result<Texture, W3dError> {
    if image.width() > 4096
        || image.height() > 4096
        || u64::from(image.width()) * u64::from(image.height()) * 4 > 64 * 1024 * 1024
    {
        return Err(W3dError::new("texture budget exceeded"));
    }
    Ok(Texture {
        image: image.to_rgba8(),
    })
}

fn missing_texture() -> Texture {
    let mut image = RgbaImage::new(8, 8);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = if (x / 2 + y / 2) % 2 == 0 {
            Rgba([255, 0, 255, 255])
        } else {
            Rgba([25, 25, 25, 255])
        };
    }
    Texture { image }
}

fn checkerboard() -> RgbaImage {
    let mut image = RgbaImage::new(WIDTH, HEIGHT);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let value = if (x / 12 + y / 12) % 2 == 0 { 96 } else { 108 };
        *pixel = Rgba([value, value, value, 255]);
    }
    image
}

fn rasterize(
    triangles: &[DrawTriangle],
    textures: &HashMap<String, Texture>,
    zoom: f32,
    pixels: &mut RgbaImage,
    depth: &mut [f32],
) -> Result<(), W3dError> {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for vertex in triangles.iter().flat_map(|triangle| &triangle.vertices) {
        min = min.min(vertex.position);
        max = max.max(vertex.position);
    }
    if !min.is_finite() || !max.is_finite() {
        return Err(W3dError::new("model contains non-finite geometry"));
    }
    let center = (min + max) * 0.5;
    let radius = triangles
        .iter()
        .flat_map(|triangle| &triangle.vertices)
        .map(|vertex| vertex.position.distance(center))
        .fold(0.0f32, f32::max)
        .max(0.01);
    let direction = Vec3::new(1.0, -1.0, 0.75).normalize();
    let fov = 35.0f32.to_radians();
    let distance = radius / (fov * 0.5).tan() * 1.15;
    let eye = center + direction * distance;
    let view = Mat4::look_at_rh(eye, center, Vec3::Z);
    let projection = Mat4::perspective_rh_gl(
        fov,
        WIDTH as f32 / HEIGHT as f32,
        (distance - radius * 1.5).max(0.001),
        distance + radius * 2.0,
    );
    let view_projection = projection * view;
    let light = Vec3::new(0.4, -0.7, 1.0).normalize();
    let mut raster_samples = MAX_RASTER_SAMPLES;

    for triangle in triangles {
        let face = (triangle.vertices[1].position - triangle.vertices[0].position)
            .cross(triangle.vertices[2].position - triangle.vertices[0].position);
        if !triangle.two_sided && face.dot(eye - triangle.vertices[0].position) <= 0.0 {
            continue;
        }
        let projected = triangle
            .vertices
            .clone()
            .map(|vertex| project(vertex, view_projection, zoom));
        if projected.iter().any(|vertex| vertex.clip_w <= 0.0) {
            continue;
        }
        draw_triangle(
            &projected,
            textures.get(&triangle.texture.to_ascii_lowercase()),
            light,
            pixels,
            depth,
            &mut raster_samples,
        )?;
    }
    Ok(())
}

#[derive(Clone)]
struct Projected {
    screen: Vec2,
    depth: f32,
    inv_w: f32,
    uv_over_w: Vec2,
    normal_over_w: Vec3,
    color_over_w: Vec4,
    clip_w: f32,
}

fn project(vertex: Vertex, view_projection: Mat4, zoom: f32) -> Projected {
    let clip = view_projection * vertex.position.extend(1.0);
    let inv_w = 1.0 / clip.w;
    let ndc = clip.truncate() * inv_w;
    Projected {
        screen: Vec2::new(
            (ndc.x * zoom * 0.5 + 0.5) * (WIDTH - 1) as f32,
            (1.0 - (ndc.y * zoom * 0.5 + 0.5)) * (HEIGHT - 1) as f32,
        ),
        depth: ndc.z,
        inv_w,
        uv_over_w: vertex.uv * inv_w,
        normal_over_w: vertex.normal * inv_w,
        color_over_w: vertex.color * inv_w,
        clip_w: clip.w,
    }
}

fn draw_triangle(
    vertices: &[Projected; 3],
    texture: Option<&Texture>,
    light: Vec3,
    pixels: &mut RgbaImage,
    depth: &mut [f32],
    raster_samples: &mut u64,
) -> Result<(), W3dError> {
    let area = edge(vertices[0].screen, vertices[1].screen, vertices[2].screen);
    if area.abs() < 0.0001 {
        return Ok(());
    }
    let min_x = vertices
        .iter()
        .map(|vertex| vertex.screen.x)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .clamp(0.0, (WIDTH - 1) as f32) as u32;
    let max_x = vertices
        .iter()
        .map(|vertex| vertex.screen.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .clamp(0.0, (WIDTH - 1) as f32) as u32;
    let min_y = vertices
        .iter()
        .map(|vertex| vertex.screen.y)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .clamp(0.0, (HEIGHT - 1) as f32) as u32;
    let max_y = vertices
        .iter()
        .map(|vertex| vertex.screen.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .clamp(0.0, (HEIGHT - 1) as f32) as u32;
    if min_x > max_x || min_y > max_y {
        return Ok(());
    }
    let samples = u64::from(max_x - min_x + 1) * u64::from(max_y - min_y + 1);
    *raster_samples = (*raster_samples)
        .checked_sub(samples)
        .ok_or_else(|| W3dError::new("model raster work budget exceeded"))?;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let point = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
            let bary = [
                edge(vertices[1].screen, vertices[2].screen, point) / area,
                edge(vertices[2].screen, vertices[0].screen, point) / area,
                edge(vertices[0].screen, vertices[1].screen, point) / area,
            ];
            if bary.iter().any(|weight| *weight < -0.0001) {
                continue;
            }
            let z = bary[0] * vertices[0].depth
                + bary[1] * vertices[1].depth
                + bary[2] * vertices[2].depth;
            let offset = (y * WIDTH + x) as usize;
            if z >= depth[offset] {
                continue;
            }
            let denominator = bary[0] * vertices[0].inv_w
                + bary[1] * vertices[1].inv_w
                + bary[2] * vertices[2].inv_w;
            if denominator <= 0.0 {
                continue;
            }
            let interpolate_vec2 = |field: fn(&Projected) -> Vec2| {
                (field(&vertices[0]) * bary[0]
                    + field(&vertices[1]) * bary[1]
                    + field(&vertices[2]) * bary[2])
                    / denominator
            };
            let interpolate_vec3 = |field: fn(&Projected) -> Vec3| {
                (field(&vertices[0]) * bary[0]
                    + field(&vertices[1]) * bary[1]
                    + field(&vertices[2]) * bary[2])
                    / denominator
            };
            let interpolate_vec4 = |field: fn(&Projected) -> Vec4| {
                (field(&vertices[0]) * bary[0]
                    + field(&vertices[1]) * bary[1]
                    + field(&vertices[2]) * bary[2])
                    / denominator
            };
            let uv = interpolate_vec2(|vertex| vertex.uv_over_w);
            let normal = interpolate_vec3(|vertex| vertex.normal_over_w).normalize_or_zero();
            let vertex_color = interpolate_vec4(|vertex| vertex.color_over_w);
            let texel = texture.map_or(Vec4::ONE, |texture| sample(texture, uv));
            let brightness = 0.35 + 0.65 * normal.dot(light).abs();
            let source = Vec4::new(
                texel.x * vertex_color.x * brightness,
                texel.y * vertex_color.y * brightness,
                texel.z * vertex_color.z * brightness,
                texel.w * vertex_color.w,
            )
            .clamp(Vec4::ZERO, Vec4::ONE);
            if source.w < 0.02 {
                continue;
            }
            let destination = pixels.get_pixel(x, y).0;
            let destination = Vec4::new(
                destination[0] as f32 / 255.0,
                destination[1] as f32 / 255.0,
                destination[2] as f32 / 255.0,
                destination[3] as f32 / 255.0,
            );
            let output = source * source.w + destination * (1.0 - source.w);
            pixels.put_pixel(
                x,
                y,
                Rgba([
                    (output.x * 255.0).round() as u8,
                    (output.y * 255.0).round() as u8,
                    (output.z * 255.0).round() as u8,
                    255,
                ]),
            );
            if source.w >= 0.99 {
                depth[offset] = z;
            }
        }
    }
    Ok(())
}

fn edge(a: Vec2, b: Vec2, point: Vec2) -> f32 {
    (point.x - a.x) * (b.y - a.y) - (point.y - a.y) * (b.x - a.x)
}

fn sample(texture: &Texture, uv: Vec2) -> Vec4 {
    let x = (uv.x.rem_euclid(1.0) * (texture.image.width() - 1) as f32).round() as u32;
    let y = (uv.y.rem_euclid(1.0) * (texture.image.height() - 1) as f32).round() as u32;
    let pixel = texture.image.get_pixel(x, y).0;
    Vec4::new(
        pixel[0] as f32 / 255.0,
        pixel[1] as f32 / 255.0,
        pixel[2] as f32 / 255.0,
        pixel[3] as f32 / 255.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{MaterialPass, Triangle};

    #[test]
    fn renders_a_bounded_png_and_reports_missing_texture() {
        let file = W3dFile {
            meshes: vec![Mesh {
                name: "Triangle".into(),
                vertices: vec![
                    Vec3::new(-1.0, 0.0, 0.0),
                    Vec3::new(1.0, 0.0, 0.0),
                    Vec3::new(0.0, 0.0, 1.0),
                ],
                normals: vec![Vec3::Y; 3],
                uvs: vec![Vec2::ZERO, Vec2::X, Vec2::Y],
                triangles: vec![Triangle { indices: [0, 1, 2] }],
                textures: vec!["missing.tga".into()],
                material_diffuse: [255; 4],
                pass: MaterialPass {
                    texture_ids: vec![0],
                    ..MaterialPass::default()
                },
                ..Mesh::default()
            }],
            ..W3dFile::default()
        };
        let rendered = thumbnail(&file, "Triangle", 1.0, |_| None).unwrap();
        let image = image::load_from_memory(&rendered.png).unwrap();
        assert_eq!((image.width(), image.height()), (WIDTH, HEIGHT));
        assert_eq!(rendered.missing_textures, vec!["missing.tga"]);

        let zoomed_out = thumbnail(&file, "Triangle", 0.5, |_| None).unwrap();
        let zoomed_in = thumbnail(&file, "Triangle", 2.0, |_| None).unwrap();
        let colored_pixels = |png: &[u8]| {
            image::load_from_memory(png)
                .unwrap()
                .to_rgb8()
                .pixels()
                .filter(|pixel| pixel[0] != pixel[1] || pixel[1] != pixel[2])
                .count()
        };
        assert!(colored_pixels(&zoomed_in.png) > colored_pixels(&zoomed_out.png));
    }

    #[test]
    fn thumbnail_fits_vscode_markdown_limit_with_noisy_texture() {
        let file = W3dFile {
            meshes: vec![Mesh {
                name: "Square".into(),
                vertices: vec![
                    Vec3::new(-1.0, 0.0, -1.0),
                    Vec3::new(1.0, 0.0, -1.0),
                    Vec3::new(1.0, 0.0, 1.0),
                    Vec3::new(-1.0, 0.0, 1.0),
                ],
                normals: vec![Vec3::NEG_Y; 4],
                uvs: vec![Vec2::ZERO, Vec2::X, Vec2::ONE, Vec2::Y],
                triangles: vec![
                    Triangle { indices: [0, 1, 2] },
                    Triangle { indices: [0, 2, 3] },
                ],
                textures: vec!["noise.tga".into()],
                material_diffuse: [255; 4],
                pass: MaterialPass {
                    texture_ids: vec![0],
                    ..MaterialPass::default()
                },
                ..Mesh::default()
            }],
            ..W3dFile::default()
        };
        let mut tga = vec![0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 32, 0x20];
        for y in 0..256u32 {
            for x in 0..256u32 {
                let value = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b);
                tga.extend_from_slice(&[value as u8, (value >> 8) as u8, (value >> 16) as u8, 255]);
            }
        }

        let rendered = thumbnail(&file, "Square", 1.0, |_| Some(tga.clone())).unwrap();
        let base64_len = rendered.png.len().div_ceil(3) * 4;
        assert!(
            base64_len + 2_048 < 100_000,
            "{} bytes of PNG becomes {base64_len} bytes of base64",
            rendered.png.len()
        );
    }

    #[test]
    fn thumbnail_rejects_excessive_triangle_and_texture_work() {
        let mesh = |triangle_count: usize, texture_count: usize| Mesh {
            name: "Budget".into(),
            vertices: vec![
                Vec3::new(-1.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
            ],
            normals: vec![Vec3::NEG_Y; 3],
            triangles: vec![Triangle { indices: [0, 1, 2] }; triangle_count],
            textures: (0..texture_count)
                .map(|index| format!("{index}.tga"))
                .collect(),
            material_diffuse: [255; 4],
            pass: MaterialPass {
                texture_ids: (0..triangle_count)
                    .map(|index| (index % texture_count.max(1)) as u32)
                    .collect(),
                ..MaterialPass::default()
            },
            ..Mesh::default()
        };

        let too_many_triangles = W3dFile {
            meshes: vec![mesh(10_001, 0)],
            ..W3dFile::default()
        };
        assert!(thumbnail(&too_many_triangles, "Budget", 1.0, |_| None).is_err());

        let too_much_raster_work = W3dFile {
            meshes: vec![mesh(500, 0)],
            ..W3dFile::default()
        };
        assert!(thumbnail(&too_much_raster_work, "Budget", 1.0, |_| None).is_err());

        let too_many_textures = W3dFile {
            meshes: vec![mesh(33, 33)],
            ..W3dFile::default()
        };
        let mut loads = 0;
        assert!(thumbnail(&too_many_textures, "Budget", 1.0, |_| {
            loads += 1;
            None
        })
        .is_err());
        assert!(loads <= 32);
    }
}
