mod rust_criu_protobuf;

use anyhow::{Context, Result};
use protobuf::Message;
use rust_criu_protobuf::rpc;
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::process::Command;

#[derive(Clone)]
pub enum CgMode {
    IGNORE = 0,
    NONE = 1,
    PROPS = 2,
    SOFT = 3,
    FULL = 4,
    STRICT = 5,
    DEFAULT = 6,
}

impl CgMode {
    pub fn from(value: i32) -> CgMode {
        match value {
            0 => Self::IGNORE,
            1 => Self::NONE,
            2 => Self::PROPS,
            3 => Self::SOFT,
            4 => Self::FULL,
            5 => Self::STRICT,
            6 => Self::DEFAULT,
            _ => Self::DEFAULT,
        }
    }
}

#[derive(Clone)]
pub struct Criu {
    criu_path: String,
    sv: [i32; 2], // socket pair

    images_dir_fd: i32,
    pid: i32,

    leave_running: Option<bool>,
    ext_unix_sk: Option<bool>,
    shell_job: Option<bool>,
    tcp_established: Option<bool>,
    file_locks: Option<bool>,
    log_level: i32,
    log_file: Option<String>,

    root: Option<String>,
    parent_img: Option<String>,
    track_mem: Option<bool>,
    auto_dedup: Option<bool>,

    work_dir_fd: i32,

    orphan_pts_master: Option<bool>,

    external_mounts: Vec<(String, String)>,
    manage_cgroups: Option<bool>,

    freeze_cgroup: Option<String>,
    cgroups_mode: Option<CgMode>,
    cgroup_props: Option<String>,
}

impl Criu {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        Criu::new_with_criu_path(String::from("criu"))
    }

    pub fn new_with_criu_path(path_to_criu: String) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            criu_path: path_to_criu,
            sv: [-1, -1],
            pid: -1,
            images_dir_fd: -1,
            log_level: -1,
            log_file: None,
            external_mounts: Vec::new(),
            orphan_pts_master: None,
            root: None,
            leave_running: None,
            ext_unix_sk: None,
            shell_job: None,
            tcp_established: None,
            file_locks: None,
            manage_cgroups: None,
            work_dir_fd: -1,
            freeze_cgroup: None,
            cgroups_mode: None,
            cgroup_props: None,
            parent_img: None,
            track_mem: None,
            auto_dedup: None,
        })
    }

    pub fn get_criu_version(&mut self) -> Result<u32, Box<dyn Error>> {
        let response = self.do_swrk_with_response(rpc::Criu_req_type::VERSION, None)?;

        let mut version: u32 = (response.version.major_number() * 10000)
            .try_into()
            .context("parsing criu version failed")?;
        version += (response.version.minor_number() * 100) as u32;
        version += response.version.sublevel() as u32;

        if response.version.has_gitid() {
            // taken from runc: if it is a git release -> increase minor by 1
            version -= version % 100;
            version += 100;
        }

        Ok(version)
    }

    fn do_swrk_with_response(
        &mut self,
        request_type: rpc::Criu_req_type,
        criu_opts: Option<rpc::Criu_opts>,
    ) -> Result<rpc::Criu_resp, Box<dyn Error>> {
        if unsafe {
            libc::socketpair(
                libc::AF_LOCAL,
                libc::SOCK_SEQPACKET,
                0,
                self.sv.as_mut_ptr(),
            ) != 0
        } {
            return Err("libc::socketpair failed".into());
        }

        let mut criu = Command::new(self.criu_path.clone())
            .arg("swrk")
            .arg(format!("{}", self.sv[1]))
            .spawn()
            .with_context(|| {
                format!(
                    "executing criu binary for swrk using path {:?} failed",
                    self.criu_path
                )
            })?;

        let mut req = rpc::Criu_req::new();
        req.set_type(request_type);

        if let Some(co) = criu_opts {
            req.opts = protobuf::MessageField::some(co);
        }

        let mut f = unsafe { File::from_raw_fd(self.sv[0]) };

        f.write_all(
            &req.write_to_bytes()
                .context("writing protobuf request to byte vec failed")?,
        )
        .with_context(|| {
            format!(
                "writing protobuf request to file (fd : {}) failed",
                self.sv[0]
            )
        })?;

        // 2*4096 taken from go-criu
        let mut buffer = [0; 2 * 4096];

        let read = f.read(&mut buffer[..]).with_context(|| {
            format!(
                "reading criu response from file (fd :{}) failed",
                self.sv[0]
            )
        })?;

        let response: rpc::Criu_resp =
            Message::parse_from_bytes(&buffer[..read]).context("parsing criu response failed")?;

        if !response.success() {
            criu.kill()
                .context("killing criu process (due to failed request) failed")?;
            return Result::Err(
                format!(
                    "CRIU RPC request failed with message:{} error:{}",
                    response.cr_errmsg(),
                    response.cr_errno()
                )
                .into(),
            );
        }

        if response.type_() != request_type {
            criu.kill()
                .context("killing criu process (due to incorrect response) failed")?;
            return Result::Err(
                format!("Unexpected CRIU RPC response ({:?})", response.type_()).into(),
            );
        }

        criu.kill().context("killing criu process failed")?;
        Result::Ok(response)
    }

    pub fn set_pid(&mut self, pid: i32) {
        self.pid = pid;
    }

    pub fn set_images_dir_fd(&mut self, fd: i32) {
        self.images_dir_fd = fd;
    }

    pub fn set_log_level(&mut self, log_level: i32) {
        self.log_level = log_level;
    }

    pub fn set_log_file(&mut self, log_file: String) {
        self.log_file = Some(log_file);
    }

    pub fn set_external_mount(&mut self, key: String, value: String) {
        self.external_mounts.push((key, value));
    }

    pub fn set_orphan_pts_master(&mut self, orphan_pts_master: bool) {
        self.orphan_pts_master = Some(orphan_pts_master);
    }

    pub fn set_root(&mut self, root: String) {
        self.root = Some(root);
    }

    pub fn set_leave_running(&mut self, leave_running: bool) {
        self.leave_running = Some(leave_running);
    }

    pub fn set_ext_unix_sk(&mut self, ext_unix_sk: bool) {
        self.ext_unix_sk = Some(ext_unix_sk);
    }

    pub fn set_shell_job(&mut self, shell_job: bool) {
        self.shell_job = Some(shell_job);
    }

    pub fn set_tcp_established(&mut self, tcp_established: bool) {
        self.tcp_established = Some(tcp_established);
    }

    pub fn set_file_locks(&mut self, file_locks: bool) {
        self.file_locks = Some(file_locks);
    }

    pub fn set_manage_cgroups(&mut self, manage_cgroups: bool) {
        self.manage_cgroups = Some(manage_cgroups);
    }

    pub fn set_work_dir_fd(&mut self, fd: i32) {
        self.work_dir_fd = fd;
    }

    pub fn set_freeze_cgroup(&mut self, freeze_cgroup: String) {
        self.freeze_cgroup = Some(freeze_cgroup);
    }

    pub fn cgroups_mode(&mut self, mode: CgMode) {
        self.cgroups_mode = Some(mode);
    }

    pub fn set_cgroup_props(&mut self, props: String) {
        self.cgroup_props = Some(props);
    }

    pub fn set_parent_img(&mut self, parent_img: String) {
        self.parent_img = Some(parent_img);
    }

    pub fn set_track_mem(&mut self, track_mem: bool) {
        self.track_mem = Some(track_mem);
    }

    pub fn set_auto_dedup(&mut self, auto_dedup: bool) {
        self.auto_dedup = Some(auto_dedup);
    }

    fn fill_criu_opts(&mut self, criu_opts: &mut rpc::Criu_opts) {
        if self.pid != -1 {
            criu_opts.set_pid(self.pid);
        }

        if self.images_dir_fd != -1 {
            criu_opts.set_images_dir_fd(self.images_dir_fd);
        }

        if self.log_level != -1 {
            criu_opts.set_log_level(self.log_level);
        }

        if self.log_file.is_some() {
            criu_opts.set_log_file(self.log_file.clone().unwrap());
        }

        if !self.external_mounts.is_empty() {
            let mut external_mounts = Vec::new();
            for e in &self.external_mounts {
                let mut external_mount = rpc::Ext_mount_map::new();
                external_mount.set_key(e.0.clone());
                external_mount.set_val(e.1.clone());
                external_mounts.push(external_mount);
            }
            self.external_mounts.clear();
            criu_opts.ext_mnt = external_mounts;
        }

        if self.orphan_pts_master.is_some() {
            criu_opts.set_orphan_pts_master(self.orphan_pts_master.unwrap());
        }

        if self.root.is_some() {
            criu_opts.set_root(self.root.clone().unwrap());
        }

        if self.leave_running.is_some() {
            criu_opts.set_leave_running(self.leave_running.unwrap());
        }

        if self.ext_unix_sk.is_some() {
            criu_opts.set_ext_unix_sk(self.ext_unix_sk.unwrap());
        }

        if self.shell_job.is_some() {
            criu_opts.set_shell_job(self.shell_job.unwrap());
        }

        if self.tcp_established.is_some() {
            criu_opts.set_tcp_established(self.tcp_established.unwrap());
        }

        if self.file_locks.is_some() {
            criu_opts.set_file_locks(self.file_locks.unwrap());
        }

        if self.manage_cgroups.is_some() {
            criu_opts.set_manage_cgroups(self.manage_cgroups.unwrap());
        }

        if self.work_dir_fd != -1 {
            criu_opts.set_work_dir_fd(self.work_dir_fd);
        }

        if self.freeze_cgroup.is_some() {
            criu_opts.set_freeze_cgroup(self.freeze_cgroup.clone().unwrap());
        }

        if self.cgroups_mode.is_some() {
            let mode = match self.cgroups_mode.as_ref().unwrap() {
                CgMode::IGNORE => rpc::Criu_cg_mode::IGNORE,
                CgMode::NONE => rpc::Criu_cg_mode::CG_NONE,
                CgMode::PROPS => rpc::Criu_cg_mode::PROPS,
                CgMode::SOFT => rpc::Criu_cg_mode::SOFT,
                CgMode::FULL => rpc::Criu_cg_mode::FULL,
                CgMode::STRICT => rpc::Criu_cg_mode::STRICT,
                CgMode::DEFAULT => rpc::Criu_cg_mode::DEFAULT,
            };
            criu_opts.set_manage_cgroups_mode(mode);
        }

        if self.cgroup_props.is_some() {
            criu_opts.set_cgroup_props(self.cgroup_props.clone().unwrap());
        }

        if self.parent_img.is_some() {
            criu_opts.set_parent_img(self.parent_img.clone().unwrap());
        }

        if self.track_mem.is_some() {
            criu_opts.set_track_mem(self.track_mem.unwrap());
        }

        if self.auto_dedup.is_some() {
            criu_opts.set_auto_dedup(self.auto_dedup.unwrap());
        }
    }

    fn clear(&mut self) {
        self.pid = -1;
        self.images_dir_fd = -1;
        self.log_level = -1;
        self.log_file = None;
        self.external_mounts = Vec::new();
        self.orphan_pts_master = None;
        self.root = None;
        self.leave_running = None;
        self.ext_unix_sk = None;
        self.shell_job = None;
        self.tcp_established = None;
        self.file_locks = None;
        self.manage_cgroups = None;
        self.work_dir_fd = -1;
        self.freeze_cgroup = None;
        self.cgroups_mode = None;
        self.cgroup_props = None;
        self.parent_img = None;
        self.track_mem = None;
        self.auto_dedup = None;
    }

    pub fn dump(&mut self) -> Result<(), Box<dyn Error>> {
        let mut criu_opts = rpc::Criu_opts::default();
        self.fill_criu_opts(&mut criu_opts);
        self.do_swrk_with_response(rpc::Criu_req_type::DUMP, Some(criu_opts))?;
        self.clear();

        Ok(())
    }

    pub fn restore(&mut self) -> Result<(), Box<dyn Error>> {
        let mut criu_opts = rpc::Criu_opts::default();
        self.fill_criu_opts(&mut criu_opts);
        self.do_swrk_with_response(rpc::Criu_req_type::RESTORE, Some(criu_opts))?;
        self.clear();

        Ok(())
    }
}

impl Drop for Criu {
    fn drop(&mut self) {
        unsafe { libc::close(self.sv[0]) };
        unsafe { libc::close(self.sv[1]) };
    }
}
