// ------------ Frame Rate Scanner ------------
// Finds where the game keeps its frame-rate value by looking for a known call pattern in its il2cpp code and
// following the jumps to the field it writes. Works on a small Image trait so tests can run on plain byte buffers.

use core::ops::Range;

const PATTERN: [u8; 6] = [0xB9, 0x3C, 0x00, 0x00, 0x00, 0xE8];

pub const SITE_LEN: usize = PATTERN.len();

const MAX_HOPS: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum ScanError {
    PatternMissing,
    Unresolved { candidates: u32 },
}

impl ScanError {
    pub fn message(&self) -> String {
        match self {
            ScanError::PatternMissing => {
                "Couldn't find the frame-rate code in this version of the game. The latest \
                 update probably changed it, so the unlocker needs a new pattern."
                    .to_string()
            }
            ScanError::Unresolved { .. } => "Found the frame-rate code but couldn't work out \
                 which value it writes. The latest update probably changed it."
                .to_string(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Resolution {
    pub rva: u32,
    pub site: u32,
    pub resolved: u32,
    pub agreeing: u32,
}

pub trait Image {
    fn len(&self) -> usize;
    fn read(&self, at: usize, out: &mut [u8]) -> bool;
    fn readable_run(&self, at: usize, end: usize) -> Option<Range<usize>>;
}

impl Image for [u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn read(&self, at: usize, out: &mut [u8]) -> bool {
        match at.checked_add(out.len()).and_then(|end| self.get(at..end)) {
            Some(bytes) => {
                out.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn readable_run(&self, at: usize, end: usize) -> Option<Range<usize>> {
        let end = end.min(<[u8]>::len(self));
        (at < end).then_some(at..end)
    }
}

const CHUNK: usize = 1 << 20;

fn read_array<const N: usize, I: Image + ?Sized>(image: &I, at: usize) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    image.read(at, &mut out).then_some(out)
}

fn read_u8<I: Image + ?Sized>(image: &I, at: usize) -> Option<u8> {
    read_array::<1, I>(image, at).map(|[byte]| byte)
}

fn read_i32<I: Image + ?Sized>(image: &I, at: usize) -> Option<i32> {
    read_array::<4, I>(image, at).map(i32::from_le_bytes)
}

fn branch_target<I: Image + ?Sized>(image: &I, at: usize) -> Option<usize> {
    let displacement = read_i32(image, at.checked_add(1)?)?;
    let end = at.checked_add(5)?;
    let target = (end as i64).checked_add(displacement as i64)?;
    let target = usize::try_from(target).ok()?;
    (target < image.len()).then_some(target)
}

fn follow_thunks<I: Image + ?Sized>(image: &I, mut at: usize) -> Option<usize> {
    for _ in 0..MAX_HOPS {
        match read_u8(image, at)? {
            0xE8 | 0xE9 => {
                let next = branch_target(image, at)?;
                if next == at {
                    return None;
                }
                at = next;
            }
            _ => return Some(at),
        }
    }
    None
}

fn rip_relative_target<I: Image + ?Sized>(image: &I, at: usize) -> Option<usize> {
    let mut at = at;
    if matches!(read_u8(image, at)?, 0x40..=0x4F) {
        at += 1;
    }
    if !matches!(read_u8(image, at)?, 0x89 | 0x8B) {
        return None;
    }
    if read_u8(image, at + 1)? & 0xC7 != 0x05 {
        return None;
    }
    let displacement = read_i32(image, at + 2)?;
    let end = at.checked_add(6)?;
    let target = (end as i64).checked_add(displacement as i64)?;
    let target = usize::try_from(target).ok()?;
    (target.checked_add(4)? <= image.len()).then_some(target)
}

fn resolve_one<I: Image + ?Sized>(image: &I, call_site: usize) -> Option<usize> {
    let call = call_site + PATTERN.len() - 1;
    if read_u8(image, branch_target(image, call)?)? != 0xE9 {
        return None;
    }
    rip_relative_target(image, follow_thunks(image, call)?)
}

fn for_each_match<I: Image + ?Sized>(image: &I, range: Range<usize>, mut found: impl FnMut(usize)) {
    let finder = memchr::memmem::Finder::new(&PATTERN);
    let overlap = PATTERN.len() - 1;
    let mut buffer = Vec::new();
    let mut at = range.start;
    while let Some(run) = image.readable_run(at, range.end) {
        let mut chunk = run.start;
        while run.end - chunk >= PATTERN.len() {
            let count = (run.end - chunk).min(CHUNK);
            buffer.resize(count, 0);
            if image.read(chunk, &mut buffer) {
                for offset in finder.find_iter(&buffer) {
                    found(chunk + offset);
                }
            }
            if chunk + count == run.end {
                break;
            }
            chunk += count - overlap;
        }
        if run.end <= at {
            break;
        }
        at = run.end;
    }
}

pub fn resolve_framerate_rva<I: Image + ?Sized>(
    image: &I,
    range: Range<usize>,
) -> Result<Resolution, ScanError> {
    let range = range.start.min(image.len())..range.end.min(image.len());

    let mut votes: Vec<(usize, u32, usize)> = Vec::new();
    let mut matched = false;
    let mut resolved = 0u32;

    for_each_match(image, range, |site| {
        matched = true;
        let Some(target) = resolve_one(image, site) else {
            return;
        };
        resolved += 1;
        match votes.iter_mut().find(|(rva, _, _)| *rva == target) {
            Some((_, count, _)) => *count += 1,
            None => votes.push((target, 1, site)),
        }
    });

    if !matched {
        return Err(ScanError::PatternMissing);
    }
    votes.sort_by_key(|&(_, count, _)| core::cmp::Reverse(count));
    let Some(&(rva, agreeing, site)) = votes.first() else {
        return Err(ScanError::Unresolved { candidates: 0 });
    };
    if votes
        .get(1)
        .is_some_and(|&(_, runner_up, _)| runner_up == agreeing)
    {
        return Err(ScanError::Unresolved {
            candidates: resolved,
        });
    }
    let unresolved = |_| ScanError::Unresolved {
        candidates: resolved,
    };
    let rva = u32::try_from(rva).map_err(unresolved)?;
    let site = u32::try_from(site).map_err(unresolved)?;
    Ok(Resolution {
        rva,
        site,
        resolved,
        agreeing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 0x400;

    fn put_branch(image: &mut [u8], at: usize, opcode: u8, target: usize) {
        image[at] = opcode;
        let rel = target as i64 - (at as i64 + 5);
        image[at + 1..at + 5].copy_from_slice(&(rel as i32).to_le_bytes());
    }

    fn put_site(image: &mut [u8], at: usize, callee: usize) {
        image[at..at + 5].copy_from_slice(&PATTERN[..5]);
        put_branch(image, at + 5, 0xE8, callee);
    }

    fn put_mov(image: &mut [u8], at: usize, opcode: u8, global: i64) {
        image[at] = 0x48;
        image[at + 1] = opcode;
        image[at + 2] = 0x05;
        let rel = global - (at as i64 + 7);
        image[at + 3..at + 7].copy_from_slice(&(rel as i32).to_le_bytes());
    }

    fn resolving_site(image: &mut [u8], site: usize, thunk: usize, mov: usize, global: usize) {
        put_site(image, site, thunk);
        put_branch(image, thunk, 0xE9, mov);
        put_mov(image, mov, 0x89, global as i64);
    }

    #[test]
    fn missing_pattern_is_reported() {
        let image = vec![0u8; LEN];
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Err(ScanError::PatternMissing)
        );
    }

    #[test]
    fn call_to_a_non_thunk_is_unresolved() {
        let mut image = vec![0u8; LEN];
        put_site(&mut image, 0x10, 0x200);
        put_mov(&mut image, 0x200, 0x89, 0x300);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Err(ScanError::Unresolved { candidates: 0 })
        );
    }

    #[test]
    fn thunk_to_a_rex_mov_resolves() {
        let mut image = vec![0u8; LEN];
        resolving_site(&mut image, 0x10, 0x100, 0x200, 0x300);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Ok(Resolution {
                rva: 0x300,
                site: 0x10,
                resolved: 1,
                agreeing: 1,
            })
        );
    }

    #[test]
    fn load_form_is_accepted() {
        let mut image = vec![0u8; LEN];
        put_site(&mut image, 0x10, 0x100);
        put_branch(&mut image, 0x100, 0xE9, 0x200);
        put_mov(&mut image, 0x200, 0x8B, 0x300);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN).map(|r| r.rva),
            Ok(0x300)
        );
    }

    #[test]
    fn self_looping_thunk_is_rejected() {
        let mut image = vec![0u8; LEN];
        put_branch(&mut image, 0x100, 0xE9, 0x100);
        assert_eq!(follow_thunks(&image[..], 0x100), None);
        put_site(&mut image, 0x10, 0x100);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Err(ScanError::Unresolved { candidates: 0 })
        );
    }

    #[test]
    fn displacement_past_the_image_is_rejected() {
        let mut image = vec![0u8; LEN];
        put_site(&mut image, 0x10, 0x100);
        put_branch(&mut image, 0x100, 0xE9, 0x200);
        put_mov(&mut image, 0x200, 0x89, LEN as i64 - 2);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Err(ScanError::Unresolved { candidates: 0 })
        );
    }

    #[test]
    fn pattern_at_the_image_end_is_unresolved() {
        let mut image = vec![0u8; LEN];
        image[LEN - PATTERN.len()..].copy_from_slice(&PATTERN);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Err(ScanError::Unresolved { candidates: 0 })
        );
    }

    #[test]
    fn pattern_outside_the_range_is_ignored() {
        let mut image = vec![0u8; LEN];
        resolving_site(&mut image, 0x10, 0x100, 0x200, 0x300);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0x80..LEN),
            Err(ScanError::PatternMissing)
        );
    }

    #[test]
    fn majority_wins_over_a_lone_site() {
        let mut image = vec![0u8; LEN];
        resolving_site(&mut image, 0x10, 0x100, 0x200, 0x310);
        resolving_site(&mut image, 0x30, 0x110, 0x210, 0x300);
        put_site(&mut image, 0x50, 0x110);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Ok(Resolution {
                rva: 0x300,
                site: 0x30,
                resolved: 3,
                agreeing: 2,
            })
        );
    }

    #[test]
    fn scanning_the_winning_site_alone_finds_the_same_field() {
        let mut image = vec![0u8; LEN];
        put_site(&mut image, 0x08, 0x100);
        resolving_site(&mut image, 0x20, 0x110, 0x200, 0x300);
        put_site(&mut image, 0x40, 0x110);
        let found = resolve_framerate_rva(&image[..], 0..LEN).expect("resolve the RVA");
        assert_eq!(found.site, 0x20);
        let site = found.site as usize;
        assert_eq!(
            resolve_framerate_rva(&image[..], site..site + SITE_LEN).map(|r| (r.rva, r.site)),
            Ok((found.rva, found.site))
        );
    }

    #[test]
    fn tied_sites_are_rejected() {
        let mut image = vec![0u8; LEN];
        resolving_site(&mut image, 0x10, 0x100, 0x200, 0x300);
        resolving_site(&mut image, 0x30, 0x110, 0x210, 0x310);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..LEN),
            Err(ScanError::Unresolved { candidates: 2 })
        );
    }

    struct Holey<'a> {
        bytes: &'a [u8],
        hole: Range<usize>,
    }

    impl Image for Holey<'_> {
        fn len(&self) -> usize {
            self.bytes.len()
        }

        fn read(&self, at: usize, out: &mut [u8]) -> bool {
            let end = at + out.len();
            (end <= self.hole.start || at >= self.hole.end) && self.bytes.read(at, out)
        }

        fn readable_run(&self, at: usize, end: usize) -> Option<Range<usize>> {
            let end = end.min(self.bytes.len());
            let at = if self.hole.contains(&at) {
                self.hole.end
            } else {
                at
            };
            let end = if at < self.hole.start {
                end.min(self.hole.start)
            } else {
                end
            };
            (at < end).then_some(at..end)
        }
    }

    #[test]
    fn thunk_into_an_unreadable_page_is_unresolved() {
        let mut image = vec![0u8; LEN];
        resolving_site(&mut image, 0x10, 0x100, 0x200, 0x300);
        let holey = Holey {
            bytes: &image,
            hole: 0x100..0x180,
        };
        assert_eq!(
            resolve_framerate_rva(&holey, 0..LEN),
            Err(ScanError::Unresolved { candidates: 0 })
        );
    }

    #[test]
    fn sites_in_an_unreadable_run_are_skipped() {
        let mut image = vec![0u8; LEN];
        resolving_site(&mut image, 0x10, 0x100, 0x200, 0x300);
        resolving_site(&mut image, 0x50, 0x110, 0x210, 0x310);
        put_site(&mut image, 0x60, 0x110);
        let holey = Holey {
            bytes: &image,
            hole: 0x40..0x80,
        };
        assert_eq!(
            resolve_framerate_rva(&holey, 0..LEN),
            Ok(Resolution {
                rva: 0x300,
                site: 0x10,
                resolved: 1,
                agreeing: 1,
            })
        );
    }

    #[test]
    fn site_across_a_chunk_boundary_is_found_once() {
        let mut image = vec![0u8; CHUNK * 2];
        resolving_site(&mut image, CHUNK - 3, 0x100, 0x200, 0x300);
        assert_eq!(
            resolve_framerate_rva(&image[..], 0..image.len()),
            Ok(Resolution {
                rva: 0x300,
                site: (CHUNK - 3) as u32,
                resolved: 1,
                agreeing: 1,
            })
        );
    }
}
