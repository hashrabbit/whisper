use memmap::{ Mmap, Protection };
use byteorder::{ ByteOrder, BigEndian, ReadBytesExt, WriteBytesExt };

mod header;
pub mod archive;

use self::header::{ Header, AggregationType };
use self::archive::{ Archive, ARCHIVE_INFO_SIZE };

use whisper::Point;
use whisper::Schema;

// Modules needed to create file on disk
use std::fs::OpenOptions;
extern crate libc;
use self::libc::funcs::posix01::unistd::ftruncate;
use std::os::unix::prelude::AsRawFd;
use std::io::{ self, Error};
use std::path::{ Path, PathBuf };

#[derive(Debug)]
pub struct WhisperFile {
	pub path: PathBuf,
	pub header: Header,
	pub archives: Vec< Archive >,
}

impl WhisperFile {
	pub fn new(path: &Path, schema: &Schema) -> io::Result<WhisperFile> {
        let mut opened_file = try!(OpenOptions::new().read(true).write(true).create(true).open(path));

		// Allocate space on disk (could be costly!)
		{
			let size_needed = schema.size_on_disk();
			let raw_fd = opened_file.as_raw_fd();
			let retval = unsafe {
				// TODO skip to fallocate-like behavior. Will need wrapper for OSX.
				ftruncate(raw_fd, size_needed as i64)
			};
			if retval != 0 {
				return Err(Error::last_os_error());
			}
		}

		let xff = 0.5;
		let header = Header::new(AggregationType::Unknown, schema.max_retention(), xff);
		{
			try!( opened_file.write_u32::<BigEndian>( header.aggregation_type.to_u32() ));
			try!( opened_file.write_u32::<BigEndian>( header.max_retention ) );
			try!( opened_file.write_f32::<BigEndian>( header.x_files_factor ) );
			try!( opened_file.write_u32::<BigEndian>( schema.retention_policies.len() as u32 ) );
		}

		let mut archive_offset = Header::archives_start( schema.retention_policies.len() ) as u32;
		for retention_policy in &schema.retention_policies {
			try!( opened_file.write_u32::<BigEndian>( archive_offset as u32 ) );
			try!( opened_file.write_u32::<BigEndian>( retention_policy.precision ) );
			try!( opened_file.write_u32::<BigEndian>( retention_policy.points()  ) );

			archive_offset = archive_offset + retention_policy.size_on_disk();
		}

		let mmap = Mmap::open(opened_file, Protection::ReadWrite ).unwrap();

		Ok( WhisperFile::open_mmap(path, mmap) )
	}

	// TODO: open should validate contents of whisper file
	// and return Result<WhisperFile, io::Error>
	pub fn open(path: &Path) -> WhisperFile {
		let mmap = Mmap::open_path(path, Protection::ReadWrite).unwrap();
		WhisperFile::open_mmap(path, mmap)
	}

	fn open_mmap<P>(path: P, mmap: Mmap) -> WhisperFile
	where P: AsRef<Path> {
		let mmap_view = mmap.into_view();

		let header = {
			let slice = unsafe{ mmap_view.as_slice() };
			Header::new_from_slice(slice)
		};
		let archives = header.mmap_to_archives(mmap_view);

		let whisper_file = WhisperFile {
			path: path.as_ref().to_path_buf(),
			header: header,
			archives: archives
		};
		whisper_file
	}

	pub fn write(&mut self, point: &Point) {
		self.archives[0].write(&point);
	}
}

#[cfg(test)]
mod tests {
	use whisper::{ Schema, WhisperFile };
	use super::header;

	use std::path::{ Path, PathBuf};
	use std::io::Cursor;
	use std::io::Write;
	use memmap::{ Mmap, Protection };

	// whisper-create.py blah.wsp 60:5
	// hexdump -v -e '"0x" 1/1 "%02X, "' blah.wsp
	const SAMPLE_FILE : [u8; 88] = [
	//  agg type
		0x00, 0x00, 0x00, 0x01,
	//  max ret
		0x00, 0x00, 0x01, 0x2C,
	// x_files_factor
		0x3F, 0x00, 0x00, 0x00,
	// archive_count
		0x00, 0x00, 0x00, 0x01,
	// archive_info[0].offset
		0x00, 0x00, 0x00, 0x1C,
	// archive_info[0].seconds_per_point
		0x00, 0x00, 0x00, 0x3C,
	// archive_info[0].points
		0x00, 0x00, 0x00, 0x05,
	// archive[0] data
		0x55, 0xD9, 0x33, 0xE8, 0x40, 0x59, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00
	];

	#[test]
	fn test_header(){
		let mut anon_mmap = Mmap::anonymous(SAMPLE_FILE.len(), Protection::ReadWrite).unwrap();
		{
			let slice : &mut [u8] = unsafe{ anon_mmap.as_mut_slice() };
			let mut cursor = Cursor::new(slice);
			cursor.write(&SAMPLE_FILE[..]).unwrap();
		};

		let hdr = header::Header::new_from_slice(unsafe{ anon_mmap.as_mut_slice() });

		assert_eq!(hdr.aggregation_type(), header::AggregationType::Unknown);
		assert_eq!(hdr.max_retention(), 300);
		assert_eq!(hdr.x_files_factor(), 0.5);

		let mmap_view = anon_mmap.into_view();
		let archives = hdr.mmap_to_archives(mmap_view);
		assert_eq!(archives.len(), 1);
		assert_eq!(archives[0].seconds_per_point(), 60);
		assert_eq!(archives[0].points(), 5);
		assert_eq!(archives[0].size(), 60); // 5 points * (8 bytes float + 4 bytes ts) = 60 bytes
	}

	#[test]
	fn test_new() {
		let path = Path::new("/tmp/blah.wsp").to_path_buf();
		let default_specs = vec!["1s:60s".to_string(), "1m:1y".to_string()];
		let schema = Schema::new_from_retention_specs(default_specs);

        let file = WhisperFile::new(&path, &schema).unwrap();
	}
}
