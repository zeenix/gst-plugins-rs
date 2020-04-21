// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use super::cea608tott_ffi as ffi;
use atomic_refcell::AtomicRefCell;

fn scale_round(val: u64, num: u64, denom: u64) -> u64 {
    unsafe { gst_sys::gst_util_uint64_scale_round(val, num, denom) }
}

fn decrement_pts(min_frame_no: u64, frame_no: &mut u64, fps_n: u64, fps_d: u64) -> (u64, u64) {
    let old_pts = scale_round(
        (*frame_no * gst::SECOND).nseconds().unwrap() as u64,
        fps_d,
        fps_n,
    );

    if *frame_no > min_frame_no {
        *frame_no -= 1;
    }

    let new_pts = scale_round(
        (*frame_no * gst::SECOND).nseconds().unwrap() as u64,
        fps_d,
        fps_n,
    );
    let duration = old_pts - new_pts;

    (new_pts, duration)
}

fn is_basicna(cc_data: u16) -> bool {
    0x0000 != (0x6000 & cc_data)
}

fn is_westeu(cc_data: u16) -> bool {
    0x1220 == (0x7660 & cc_data)
}

fn is_specialna(cc_data: u16) -> bool {
    0x1130 == (0x7770 & cc_data)
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn eia608_from_utf8_1(c: &[u8; 5]) -> u16 {
    assert!(c[4] == 0);
    unsafe { ffi::eia608_from_utf8_1(c.as_ptr() as *const _, 0) }
}

fn eia608_row_column_preamble(row: i32, col: i32) -> u16 {
    unsafe {
        /* Hardcoded chan and underline */
        ffi::eia608_row_column_pramble(row, col, 0, 0)
    }
}

fn eia608_control_command(cmd: ffi::eia608_control_t) -> u16 {
    unsafe { ffi::eia608_control_command(cmd, 0) }
}

fn eia608_from_basicna(bna1: u16, bna2: u16) -> u16 {
    unsafe { ffi::eia608_from_basicna(bna1, bna2) }
}

fn buffer_from_cc_data(cc_data: u16) -> gst::buffer::Buffer {
    let mut ret = gst::Buffer::with_size(2).unwrap();
    {
        let buf_mut = ret.get_mut().unwrap();

        let cc_data = cc_data.to_be_bytes();

        gst_trace!(CAT, "CC data: {:x} {:x}", cc_data[0], cc_data[1]);

        buf_mut.copy_from_slice(0, &cc_data).unwrap();
    }

    ret
}

fn control_command_buffer(buffers: &mut Vec<gst::Buffer>, cmd: ffi::eia608_control_t) {
    let cc_data = eia608_control_command(cmd);
    buffers.push(buffer_from_cc_data(cc_data));
    buffers.push(buffer_from_cc_data(cc_data));
}

fn erase_non_displayed_memory(buffers: &mut Vec<gst::Buffer>) {
    control_command_buffer(
        buffers,
        ffi::eia608_control_t_eia608_control_erase_non_displayed_memory,
    );
}

fn erase_display_memory(
    bufferlist: &mut gst::BufferListRef,
    pts: gst::ClockTime,
    duration: gst::ClockTime,
) {
    let cc_data = eia608_control_command(ffi::eia608_control_t_eia608_control_erase_display_memory);

    let mut buffer = buffer_from_cc_data(cc_data);
    {
        let buf_mut = buffer.get_mut().unwrap();
        buf_mut.set_pts(pts);
        buf_mut.set_duration(duration);
    }
    bufferlist.insert(0, buffer);
}

fn resume_caption_loading(buffers: &mut Vec<gst::Buffer>) {
    control_command_buffer(
        buffers,
        ffi::eia608_control_t_eia608_control_resume_caption_loading,
    );
}

fn end_of_caption(buffers: &mut Vec<gst::Buffer>) {
    control_command_buffer(buffers, ffi::eia608_control_t_eia608_control_end_of_caption);
}

fn preamble_buffer(buffers: &mut Vec<gst::Buffer>, row: i32, col: i32) {
    let cc_data = eia608_row_column_preamble(row, col);
    buffers.push(buffer_from_cc_data(cc_data));
    buffers.push(buffer_from_cc_data(cc_data));
}

fn bna_buffer(buffers: &mut Vec<gst::Buffer>, bna1: u16, bna2: u16) {
    let cc_data = eia608_from_basicna(bna1, bna2);

    buffers.push(buffer_from_cc_data(cc_data));
}

const DEFAULT_FPS_N: i32 = 30;
const DEFAULT_FPS_D: i32 = 1;

/* 74 is quite the magic number:
 * 2 byte pairs for resume_caption_loading
 * 2 byte pairs for erase_non_displayed_memory
 * At most 4 byte pairs for the preambles (one per line, at most 2 lines)
 * At most 64 byte pairs for the text if it's made up of 64 westeu characters
 * At most 2 byte pairs if we need to splice in an erase_display_memory
 */
const LATENCY_BUFFERS: u64 = 74;

struct State {
    framerate: gst::Fraction,
    erase_display_frame_no: Option<u64>,
    last_frame_no: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            framerate: gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
            erase_display_frame_no: None,
            last_frame_no: 0,
        }
    }
}

struct TtToCea608 {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    state: AtomicRefCell<State>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "tttocea608",
        gst::DebugColorFlags::empty(),
        Some("TT CEA 608 Element"),
    );
    static ref SPACE: u16 = eia608_from_utf8_1(&[0x20, 0, 0, 0, 0]);
}

impl TtToCea608 {
    fn push_list(
        &self,
        bufferlist: gst::BufferList,
        last_frame_no: u64,
        new_frame_no: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if last_frame_no != new_frame_no {
            let state = self.state.borrow_mut();
            let (fps_n, fps_d) = (
                *state.framerate.numer() as u64,
                *state.framerate.denom() as u64,
            );
            let start: gst::ClockTime = scale_round(
                (last_frame_no * gst::SECOND).nseconds().unwrap() as u64,
                fps_d,
                fps_n,
            )
            .into();
            let end: gst::ClockTime = scale_round(
                (new_frame_no * gst::SECOND).nseconds().unwrap() as u64,
                fps_d,
                fps_n,
            )
            .into();

            let event = gst::Event::new_gap(start, end - start).build();

            drop(state);

            let _ = self.srcpad.push_event(event);
        }
        self.srcpad.push_list(bufferlist)
    }

    fn do_erase_display(
        &self,
        min_frame_no: u64,
        mut erase_display_frame_no: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        let (fps_n, fps_d) = (
            *state.framerate.numer() as u64,
            *state.framerate.denom() as u64,
        );

        let mut bufferlist = gst::BufferList::new();

        state.last_frame_no = erase_display_frame_no;

        let (pts, duration) =
            decrement_pts(min_frame_no, &mut erase_display_frame_no, fps_n, fps_d);
        erase_display_memory(bufferlist.get_mut().unwrap(), pts.into(), duration.into());
        let (pts, duration) =
            decrement_pts(min_frame_no, &mut erase_display_frame_no, fps_n, fps_d);
        erase_display_memory(bufferlist.get_mut().unwrap(), pts.into(), duration.into());

        drop(state);

        self.push_list(bufferlist, min_frame_no, erase_display_frame_no)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut row = 13;
        let mut col = 0;

        let pts = match buffer.get_pts() {
            gst::CLOCK_TIME_NONE => {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Stream with timestamped buffers required"]
                );
                Err(gst::FlowError::Error)
            }
            pts => Ok(pts),
        }?;

        let duration = match buffer.get_duration() {
            gst::CLOCK_TIME_NONE => {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Buffers of stream need to have a duration"]
                );
                Err(gst::FlowError::Error)
            }
            duration => Ok(duration),
        }?;

        let mut state = self.state.borrow_mut();
        let mut buffers = vec![];

        {
            resume_caption_loading(&mut buffers);
            erase_non_displayed_memory(&mut buffers);
            preamble_buffer(&mut buffers, row, 0);

            let data = buffer.map_readable().map_err(|_| {
                gst_error!(CAT, obj: pad, "Can't map buffer readable");

                gst::FlowError::Error
            })?;

            let data = std::str::from_utf8(&data).map_err(|err| {
                gst_error!(CAT, obj: pad, "Can't decode utf8: {}", err);

                gst::FlowError::Error
            })?;

            let mut prev_char: u16 = 0;
            for c in data.chars() {
                if c == '\n' {
                    if prev_char != 0 {
                        buffers.push(buffer_from_cc_data(prev_char));
                        prev_char = 0;
                    }

                    row += 1;

                    if row > 14 {
                        break;
                    }

                    preamble_buffer(&mut buffers, row, 0);

                    col = 0;
                    continue;
                } else if c == '\r' {
                    continue;
                }

                let mut encoded = [0; 5];
                c.encode_utf8(&mut encoded);
                let mut cc_data = eia608_from_utf8_1(&encoded);

                if cc_data == 0 {
                    gst_warning!(CAT, obj: element, "Not translating UTF8: {}", c);
                    cc_data = *SPACE;
                }

                if is_basicna(prev_char) {
                    if is_basicna(cc_data) {
                        bna_buffer(&mut buffers, prev_char, cc_data);
                    } else if is_westeu(cc_data) {
                        // extended characters overwrite the previous character,
                        // so insert a dummy char then write the extended char
                        bna_buffer(&mut buffers, prev_char, *SPACE);
                        buffers.push(buffer_from_cc_data(cc_data));
                    } else {
                        buffers.push(buffer_from_cc_data(prev_char));
                        buffers.push(buffer_from_cc_data(cc_data));
                    }
                    prev_char = 0;
                } else if is_westeu(cc_data) {
                    // extended characters overwrite the previous character,
                    // so insert a dummy char then write the extended char
                    buffers.push(buffer_from_cc_data(*SPACE));
                    buffers.push(buffer_from_cc_data(cc_data));
                } else if is_basicna(cc_data) {
                    prev_char = cc_data;
                } else {
                    buffers.push(buffer_from_cc_data(cc_data));
                }

                if is_specialna(cc_data) {
                    resume_caption_loading(&mut buffers);
                }

                col += 1;

                if col > 32 {
                    gst_warning!(
                        CAT,
                        obj: element,
                        "Dropping character after 32nd column: {}",
                        c
                    );
                    continue;
                }
            }

            if prev_char != 0 {
                buffers.push(buffer_from_cc_data(prev_char));
            }

            end_of_caption(&mut buffers);
        }

        let mut bufferlist = gst::BufferList::new();

        let (fps_n, fps_d) = (
            *state.framerate.numer() as u64,
            *state.framerate.denom() as u64,
        );

        /* Calculate the frame for which we want the first of our
         * (doubled) end_of_caption control codes to be output
         */
        let mut frame_no =
            scale_round(pts.nseconds().unwrap(), fps_n, fps_d) / gst::SECOND.nseconds().unwrap();

        let mut erase_display_frame_no = {
            if state.erase_display_frame_no < Some(frame_no) {
                state.erase_display_frame_no
            } else {
                None
            }
        };

        /* Add 2: One for our second end_of_caption control
         * code, another to calculate its duration */
        frame_no += 2;

        /* Store that frame number, so we can make sure not to output
         * overlapped timestamps, outputting multiple buffers with
         * a 0 duration will break strict line-21 encoding, but
         * we should be fine with 608 over 708, as we can encode
         * multiple byte pairs into a single frame */
        let mut min_frame_no = state.last_frame_no;
        state.last_frame_no = frame_no;

        state.erase_display_frame_no = Some(
            scale_round((pts + duration).nseconds().unwrap(), fps_n, fps_d)
                / gst::SECOND.nseconds().unwrap()
                + 2,
        );

        for mut buffer in buffers.drain(..).rev() {
            /* Insert display erasure at the correct moment */
            if erase_display_frame_no == Some(frame_no) {
                let (pts, duration) = decrement_pts(min_frame_no, &mut frame_no, fps_n, fps_d);
                erase_display_memory(bufferlist.get_mut().unwrap(), pts.into(), duration.into());
                let (pts, duration) = decrement_pts(min_frame_no, &mut frame_no, fps_n, fps_d);
                erase_display_memory(bufferlist.get_mut().unwrap(), pts.into(), duration.into());

                erase_display_frame_no = None;
            }

            let (pts, duration) = decrement_pts(min_frame_no, &mut frame_no, fps_n, fps_d);

            let buf_mut = buffer.get_mut().unwrap();
            buf_mut.set_pts(pts.into());
            buf_mut.set_duration(duration.into());
            bufferlist.get_mut().unwrap().insert(0, buffer);
        }

        drop(state);

        if let Some(erase_display_frame_no) = erase_display_frame_no {
            self.do_erase_display(min_frame_no, erase_display_frame_no)?;
            min_frame_no = erase_display_frame_no;
        }

        self.push_list(bufferlist, min_frame_no, frame_no)
            .map_err(|err| {
                gst_error!(CAT, obj: &self.srcpad, "Pushing buffer returned {:?}", err);
                err
            })
    }

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Query::new_latency();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let state = self.state.borrow();
                    let (live, mut min, mut max) = peer_query.get_result();
                    let (fps_n, fps_d) = (
                        *state.framerate.numer() as u64,
                        *state.framerate.denom() as u64,
                    );

                    let our_latency: gst::ClockTime = scale_round(
                        (LATENCY_BUFFERS * gst::SECOND).nseconds().unwrap(),
                        fps_d,
                        fps_n,
                    )
                    .into();

                    min += our_latency;
                    max += our_latency;

                    q.set(live, min, max);
                }
                ret
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        use gst::EventView;

        match event.view() {
            EventView::Caps(..) => {
                let mut downstream_caps = match self.srcpad.get_allowed_caps() {
                    None => self.srcpad.get_pad_template_caps().unwrap(),
                    Some(caps) => caps,
                };

                if downstream_caps.is_empty() {
                    gst_error!(CAT, obj: pad, "Empty downstream caps");
                    return false;
                }

                let caps = downstream_caps.make_mut();
                let s = caps.get_mut_structure(0).unwrap();

                s.fixate_field_nearest_fraction(
                    "framerate",
                    gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
                );
                s.fixate();

                let mut state = self.state.borrow_mut();
                state.framerate = s.get_some::<gst::Fraction>("framerate").unwrap();

                gst_debug!(CAT, obj: pad, "Pushing caps {}", caps);

                let new_event = gst::Event::new_caps(&downstream_caps).build();

                drop(state);

                return self.srcpad.push_event(new_event);
            }
            EventView::Gap(e) => {
                let mut state = self.state.borrow_mut();
                let (fps_n, fps_d) = (
                    *state.framerate.numer() as u64,
                    *state.framerate.denom() as u64,
                );

                let (timestamp, duration) = e.get();
                let mut frame_no =
                    scale_round((timestamp + duration).nseconds().unwrap(), fps_n, fps_d)
                        / gst::SECOND.nseconds().unwrap();

                if frame_no < LATENCY_BUFFERS {
                    return true;
                }

                frame_no -= LATENCY_BUFFERS;

                if let Some(erase_display_frame_no) = state.erase_display_frame_no {
                    if erase_display_frame_no <= frame_no {
                        let min_frame_no = state.last_frame_no;
                        state.erase_display_frame_no = None;

                        drop(state);

                        /* Ignore return value, we may be flushing here and can't
                         * communicate that through a boolean
                         */
                        let _ = self.do_erase_display(min_frame_no, erase_display_frame_no);
                    }
                }

                return true;
            }
            EventView::Eos(_) => {
                let mut state = self.state.borrow_mut();
                if let Some(erase_display_frame_no) = state.erase_display_frame_no {
                    let min_frame_no = state.last_frame_no;
                    state.erase_display_frame_no = None;

                    drop(state);

                    /* Ignore return value, we may be flushing here and can't
                     * communicate that through a boolean
                     */
                    let _ = self.do_erase_display(min_frame_no, erase_display_frame_no);
                }
            }
            _ => (),
        }

        pad.event_default(Some(element), event)
    }
}

impl ObjectSubclass for TtToCea608 {
    const NAME: &'static str = "TtToCea608";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        sinkpad.set_chain_function(|pad, parent, buffer| {
            TtToCea608::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |this, element| this.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            TtToCea608::catch_panic_pad_function(
                parent,
                || false,
                |this, element| this.sink_event(pad, element, event),
            )
        });
        srcpad.set_query_function(|pad, parent, query| {
            TtToCea608::catch_panic_pad_function(
                parent,
                || false,
                |this, element| this.src_query(pad, element, query),
            )
        });

        sinkpad.use_fixed_caps();
        srcpad.use_fixed_caps();

        Self {
            srcpad,
            sinkpad,
            state: AtomicRefCell::new(State::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "TT to CEA-608",
            "Generic",
            "Converts timed text to CEA-608 Closed Captions",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

        let caps = gst::Caps::builder("text/x-raw").build();

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let framerate = gst::FractionRange::new(
            gst::Fraction::new(1, std::i32::MAX),
            gst::Fraction::new(std::i32::MAX, 1),
        );

        let caps = gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field("framerate", &framerate)
            .build();

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);
    }
}

impl ObjectImpl for TtToCea608 {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for TtToCea608 {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        let ret = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        Ok(ret)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "tttocea608",
        gst::Rank::None,
        TtToCea608::get_type(),
    )
}
