//! Coefficient-only JPEG probe for calibrating hardware YUV422 byte packing.
//! No decoder color matrix, format-enum assumption, heap, or pixel buffer is used.

pub(super) const WIDTH: usize = 32;
pub(super) const HEIGHT: usize = 16;

/// Semantic [Y0, Cb, Y1, Cr], independent of the hardware's byte packing.
/// Coordinates are pixels in the probe image; every pair starts dark then bright.
pub(super) fn pair(x: usize, y: usize) -> [u8; 4] {
    match (x / 16, y / 8) {
        (0, 0) => [80, 96, 152, 160],
        (1, 0) => [104, 160, 176, 96],
        (0, 1) => [88, 96, 160, 96],
        (1, 1) => [72, 160, 144, 160],
        _ => panic!("probe coordinates out of bounds"),
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self.pos.checked_add(len).ok_or("JPEG length overflow")?;
        let bytes = self.data.get(self.pos..end).ok_or("truncated JPEG")?;
        self.pos = end;
        Ok(bytes)
    }

    fn marker(&mut self) -> Result<u8, &'static str> {
        if self.take(1)?[0] != 0xff {
            return Err("expected JPEG marker");
        }
        loop {
            let marker = self.take(1)?[0];
            if marker == 0xff {
                continue;
            }
            if marker == 0 {
                return Err("stuffed byte outside entropy");
            }
            return Ok(marker);
        }
    }

    fn segment(&mut self) -> Result<&'a [u8], &'static str> {
        let length = self.take(2)?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        if length < 2 {
            return Err("invalid JPEG segment length");
        }
        self.take(length - 2)
    }
}

#[derive(Clone, Copy)]
struct Huffman<'a> {
    counts: &'a [u8],
    symbols: &'a [u8],
}

impl Huffman<'_> {
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u8, &'static str> {
        let mut code = 0_u32;
        let mut first = 0_u32;
        let mut offset = 0_usize;
        for &count in self.counts {
            code = (code << 1) | bits.read(1)?;
            let count = u32::from(count);
            if code >= first && code - first < count {
                return Ok(self.symbols[offset + (code - first) as usize]);
            }
            first = (first + count) << 1;
            offset += count as usize;
        }
        Err("invalid Huffman code")
    }
}

struct Bits<'a> {
    cursor: Cursor<'a>,
    byte: u8,
    left: u8,
}

impl Bits<'_> {
    fn read(&mut self, count: u8) -> Result<u32, &'static str> {
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                self.byte = self.cursor.take(1)?[0];
                if self.byte == 0xff && self.cursor.take(1)?[0] != 0 {
                    return Err("unexpected marker in entropy");
                }
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.byte >> self.left) & 1);
        }
        Ok(value)
    }

    fn signed(&mut self, size: u8) -> Result<i32, &'static str> {
        if size == 0 {
            return Ok(0);
        }
        let value = self.read(size)? as i32;
        Ok(if value < (1 << (size - 1)) {
            value - ((1 << size) - 1)
        } else {
            value
        })
    }

    fn finish(mut self) -> Result<(), &'static str> {
        let mask = (1_u16 << self.left) - 1;
        if u16::from(self.byte) & mask != mask {
            return Err("invalid entropy padding");
        }
        if self.cursor.marker()? != 0xd9 {
            return Err("expected EOI after single scan");
        }
        // Bytes after EOI may belong to an enclosing buffer, not this JPEG.
        Ok(())
    }
}

fn quantization(segment: &[u8], quant: &mut [Option<u16>; 4]) -> Result<(), &'static str> {
    let mut cursor = Cursor {
        data: segment,
        pos: 0,
    };
    if segment.is_empty() {
        return Err("empty DQT");
    }
    while cursor.pos < segment.len() {
        let info = cursor.take(1)?[0];
        let precision = info >> 4;
        let id = usize::from(info & 15);
        if precision > 1 || id >= quant.len() {
            return Err("invalid DQT selector");
        }
        let values = cursor.take(64 * (usize::from(precision) + 1))?;
        let mut dc = 0;
        for index in 0..64 {
            let value = if precision == 0 {
                u16::from(values[index])
            } else {
                u16::from_be_bytes([values[index * 2], values[index * 2 + 1]])
            };
            if value == 0 {
                return Err("zero quantization value");
            }
            if index == 0 {
                dc = value;
            }
        }
        quant[id] = Some(dc);
    }
    Ok(())
}

fn huffman<'a>(
    segment: &'a [u8],
    dc: &mut [Option<Huffman<'a>>; 4],
    ac: &mut [Option<Huffman<'a>>; 4],
) -> Result<(), &'static str> {
    let mut cursor = Cursor {
        data: segment,
        pos: 0,
    };
    if segment.is_empty() {
        return Err("empty DHT");
    }
    while cursor.pos < segment.len() {
        let info = cursor.take(1)?[0];
        let class = info >> 4;
        let id = usize::from(info & 15);
        if class > 1 || id >= 4 {
            return Err("invalid DHT selector");
        }
        let counts = cursor.take(16)?;
        let mut slots = 1_i32;
        let mut total = 0_usize;
        for &count in counts {
            slots = slots * 2 - i32::from(count);
            // JPEG forbids an all-ones code, including a completely filled tree.
            if slots <= 0 {
                return Err("oversubscribed or all-ones Huffman table");
            }
            total += usize::from(count);
        }
        if total == 0 || total > 256 {
            return Err("invalid Huffman symbol count");
        }
        let symbols = cursor.take(total)?;
        let mut seen = [false; 256];
        for &symbol in symbols {
            if seen[usize::from(symbol)] {
                return Err("duplicate Huffman symbol");
            }
            seen[usize::from(symbol)] = true;
            if class == 0 {
                if symbol > 11 {
                    return Err("invalid baseline DC symbol");
                }
            } else if symbol & 15 > 10 || (symbol & 15 == 0 && symbol != 0 && symbol != 0xf0) {
                return Err("invalid baseline AC symbol");
            }
        }
        let table = Some(Huffman { counts, symbols });
        if class == 0 {
            dc[id] = table;
        } else {
            ac[id] = table;
        }
    }
    Ok(())
}

fn frame(segment: &[u8]) -> Result<[usize; 3], &'static str> {
    if segment.len() != 15 || segment[0] != 8 || segment[5] != 3 {
        return Err("expected 8-bit three-component SOF0");
    }
    let height = u16::from_be_bytes([segment[1], segment[2]]);
    let width = u16::from_be_bytes([segment[3], segment[4]]);
    if usize::from(width) != WIDTH || usize::from(height) != HEIGHT {
        return Err("wrong probe dimensions");
    }
    let mut quant = [0; 3];
    let mut seen = [false; 3];
    for component in segment[6..].chunks_exact(3) {
        let id = component[0];
        if !(1..=3).contains(&id) {
            return Err("expected YCbCr component IDs 1, 2, 3");
        }
        let index = usize::from(id - 1);
        if seen[index] || component[1] != if index == 0 { 0x21 } else { 0x11 } {
            return Err("expected YCbCr 4:2:2 sampling");
        }
        if component[2] > 3 {
            return Err("invalid frame quantization selector");
        }
        seen[index] = true;
        quant[index] = usize::from(component[2]);
    }
    Ok(quant)
}

fn scan(segment: &[u8]) -> Result<[(usize, usize, usize); 3], &'static str> {
    if segment.len() != 10 || segment[0] != 3 || segment[7..] != [0, 63, 0] {
        return Err("expected single interleaved baseline scan");
    }
    let mut selectors = [(0, 0, 0); 3];
    let mut seen = [false; 3];
    for (slot, component) in segment[1..7].chunks_exact(2).enumerate() {
        let id = component[0];
        if !(1..=3).contains(&id) {
            return Err("invalid scan component ID");
        }
        let index = usize::from(id - 1);
        let dc = usize::from(component[1] >> 4);
        let ac = usize::from(component[1] & 15);
        if seen[index] || dc >= 4 || ac >= 4 {
            return Err("invalid scan table selector");
        }
        seen[index] = true;
        selectors[slot] = (index, dc, ac);
    }
    Ok(selectors)
}

fn block(
    bits: &mut Bits<'_>,
    dc: Huffman<'_>,
    ac: Huffman<'_>,
    predictor: &mut i32,
) -> Result<i32, &'static str> {
    let size = dc.decode(bits)?;
    *predictor += bits.signed(size)?;
    if !(-1024..=1023).contains(predictor) {
        return Err("baseline DC predictor out of range");
    }
    let mut index = 1_usize;
    let mut horizontal = 0;
    while index < 64 {
        let symbol = ac.decode(bits)?;
        if symbol == 0 {
            break;
        }
        if symbol == 0xf0 {
            index += 16;
            if index > 64 {
                return Err("AC zero run exceeds block");
            }
            continue;
        }
        index += usize::from(symbol >> 4);
        if index >= 64 {
            return Err("AC coefficient exceeds block");
        }
        let value = bits.signed(symbol & 15)?;
        if index == 1 {
            horizontal = value;
        }
        index += 1;
    }
    Ok(horizontal)
}

/// Lower is better: mean absolute block-average error, with 64 added to the
/// error of each Y block whose first horizontal AC coefficient is nonnegative.
/// Fractional DC averages are retained until the final integer division over
/// all 16 block values. Only a 32x16, single-scan baseline YCbCr 4:2:2 JPEG is
/// supported; progressive coding and nonzero restart intervals are rejected.
/// Header tables are borrowed from `jpeg`; no memory allocation is performed.
pub(super) fn score(jpeg: &[u8]) -> Result<u32, &'static str> {
    let mut cursor = Cursor { data: jpeg, pos: 0 };
    if cursor.take(2)? != [0xff, 0xd8] {
        return Err("missing SOI");
    }
    let mut quant = [None; 4];
    let mut dc = [None; 4];
    let mut ac = [None; 4];
    let mut frame_quant = None;
    let selectors;
    loop {
        match cursor.marker()? {
            0xdb => quantization(cursor.segment()?, &mut quant)?,
            0xc4 => huffman(cursor.segment()?, &mut dc, &mut ac)?,
            0xc0 => {
                if frame_quant.is_some() {
                    return Err("duplicate SOF0");
                }
                frame_quant = Some(frame(cursor.segment()?)?);
            }
            0xdd => {
                if cursor.segment()? != [0, 0] {
                    return Err("restart intervals unsupported");
                }
            }
            0xda => {
                selectors = scan(cursor.segment()?)?;
                break;
            }
            0xe0..=0xef | 0xfe => {
                cursor.segment()?;
            }
            _ => return Err("unsupported JPEG marker"),
        }
    }
    let frame_quant = frame_quant.ok_or("missing SOF0")?;
    for &(component, dc_id, ac_id) in &selectors {
        if quant[frame_quant[component]].is_none() || dc[dc_id].is_none() || ac[ac_id].is_none() {
            return Err("missing JPEG table");
        }
    }
    let mut bits = Bits {
        cursor,
        byte: 0,
        left: 0,
    };
    let mut predictors = [0; 3];
    let mut error = 0_u64;
    for mcu_y in 0..HEIGHT / 8 {
        for mcu_x in 0..WIDTH / 16 {
            let [y0, cb, y1, cr] = pair(mcu_x * 16, mcu_y * 8);
            let expected = [
                (i32::from(y0) + i32::from(y1)) / 2,
                i32::from(cb),
                i32::from(cr),
            ];
            for &(component, dc_id, ac_id) in &selectors {
                // Presence was checked before entropy decoding.
                let dc_table = dc[dc_id].ok_or("missing DC table")?;
                let ac_table = ac[ac_id].ok_or("missing AC table")?;
                let q = i32::from(quant[frame_quant[component]].ok_or("missing DQT")?);
                for _ in 0..if component == 0 { 2 } else { 1 } {
                    let horizontal =
                        block(&mut bits, dc_table, ac_table, &mut predictors[component])?;
                    let average_eighths = 128 * 8 + predictors[component] * q;
                    error += u64::from((average_eighths - expected[component] * 8).unsigned_abs());
                    if component == 0 && horizontal >= 0 {
                        error += 64 * 8;
                    }
                }
            }
        }
    }
    bits.finish()?;
    Ok((error / (16 * 8)) as u32)
}
