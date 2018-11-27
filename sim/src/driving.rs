use abstutil;
use abstutil::{deserialize_btreemap, serialize_btreemap, Error};
use dimensioned::si;
use geom::EPSILON_DIST;
use intersections::{IntersectionSimState, Request};
use kinematics;
use kinematics::Vehicle;
use map_model::{
    BuildingID, IntersectionID, LaneID, Map, Path, PathStep, Position, Trace, Traversable, TurnID,
    LANE_THICKNESS,
};
use multimap::MultiMap;
use ordered_float::NotNaN;
use parking::ParkingSimState;
use rand::XorShiftRng;
use router::Router;
use std;
use std::collections::{BTreeMap, HashSet};
use transit::TransitSimState;
use view::{AgentView, WorldView};
use {
    Acceleration, AgentID, CarID, CarState, Distance, DrawCarInput, Event, ParkedCar, ParkingSpot,
    Speed, Tick, Time, TripID, VehicleType,
};

const TIME_TO_PARK_OR_DEPART: Time = si::Second {
    value_unsafe: 10.0,
    _marker: std::marker::PhantomData,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrivingGoal {
    ParkNear(BuildingID),
    Border(IntersectionID, LaneID),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct ParkingState {
    // False means departing
    is_parking: bool,
    started_at: Tick,
    tuple: ParkedCar,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Car {
    id: CarID,
    trip: TripID,
    owner: Option<BuildingID>,
    on: Traversable,
    speed: Speed,
    dist_along: Distance,
    vehicle: Vehicle,

    parking: Option<ParkingState>,

    debug: bool,
}

pub enum Action {
    StartParking(ParkingSpot),
    WorkOnParking,
    StartParkingBike,
    Continue(Acceleration, Vec<Request>),
    // TODO Get rid of this one
    VanishAtDeadEnd,
}

impl Car {
    // Note this doesn't change the car's state, and it observes a fixed view of the world!
    fn react(
        &self,
        events: &mut Vec<Event>,
        // The high-level plan might change here.
        orig_router: &mut Router,
        rng: &mut XorShiftRng,
        map: &Map,
        time: Tick,
        view: &WorldView,
        parking_sim: &ParkingSimState,
        // State transitions might be indicated
        transit_sim: &mut TransitSimState,
        intersections: &IntersectionSimState,
    ) -> Result<Action, Error> {
        if self.parking.is_some() {
            // TODO right place for this check?
            assert!(self.speed <= kinematics::EPSILON_SPEED);
            return Ok(Action::WorkOnParking);
        }

        let vehicle = &self.vehicle;

        if let Some(act) = orig_router.react_before_lookahead(
            events,
            view.get_car(self.id),
            vehicle,
            time,
            map,
            parking_sim,
            transit_sim,
            rng,
        ) {
            return Ok(act);
        }

        // Use the speed limit of the current road for lookahead, including figuring out
        // accel_to_follow. If we use the value from later lookahead lanes and it's lower, than our
        // current speed will exceed it and cause issues. We guarantee we'll be slow enough by
        // entry time to that next lane. This might make accel_to_follow too pessimistic.
        let orig_speed_limit = vehicle.clamp_speed(self.on.speed_limit(map));

        // TODO could wrap this state up
        let mut dist_to_lookahead = vehicle.max_lookahead_dist(self.speed, orig_speed_limit)?
            + Vehicle::worst_case_following_dist();
        // TODO when we add stuff here, optionally log stuff?
        let mut constraints: Vec<Acceleration> = Vec::new();
        let mut requests: Vec<Request> = Vec::new();
        let mut current_on = self.on;
        let mut current_dist_along = self.dist_along;
        let mut current_router = orig_router.clone();
        let mut dist_scanned_ahead = 0.0 * si::M;

        loop {
            if self.debug {
                debug!(
                    "  -- At {}, {} looking ahead to {:?} with {} left to scan",
                    time, self.id, current_on, dist_to_lookahead
                );
            }

            // Don't exceed the speed limit
            {
                let current_speed_limit = vehicle.clamp_speed(current_on.speed_limit(map));
                let accel =
                    vehicle.accel_to_achieve_speed_in_one_tick(self.speed, current_speed_limit);
                constraints.push(accel);
                if self.debug {
                    debug!(
                        "  {} needs {} to match speed limit of {}",
                        self.id, accel, current_speed_limit
                    );
                }
            }

            // Don't hit the vehicle in front of us
            if let Some(other) = view.next_car_in_front_of(current_on, current_dist_along) {
                assert!(self.id != other.id.as_car());
                assert!(current_dist_along < other.dist_along);
                let other_vehicle = other.vehicle.as_ref().unwrap();
                let dist_behind_other =
                    dist_scanned_ahead + (other.dist_along - current_dist_along);
                // If our lookahead doesn't even hit the lead vehicle (plus following distance!!!), then ignore them.
                let total_scanning_dist =
                    dist_scanned_ahead + dist_to_lookahead + other_vehicle.following_dist();
                if total_scanning_dist >= dist_behind_other {
                    let accel = vehicle.accel_to_follow(
                        self.speed,
                        orig_speed_limit,
                        other_vehicle,
                        dist_behind_other,
                        other.speed,
                    )?;

                    if self.debug {
                        debug!(
                            "  {} needs {} to not hit {}. Currently {} behind them",
                            self.id, accel, other.id, dist_behind_other
                        );
                    }

                    constraints.push(accel);
                } else if self.debug {
                    debug!("  {} is {} behind {}. Scanned ahead so far {} + lookahead dist {} + following dist {} = {} is less than that, so ignore them", self.id, dist_behind_other, other.id, dist_scanned_ahead, dist_to_lookahead, other_vehicle.following_dist(), total_scanning_dist);
                }
            }

            // Stop for something?
            if current_on.maybe_lane().is_some() {
                let maybe_stop_early = current_router.stop_early_at_dist(
                    current_on,
                    current_dist_along,
                    vehicle,
                    map,
                    parking_sim,
                    transit_sim,
                );
                let dist_to_maybe_stop_at = maybe_stop_early.unwrap_or(current_on.length(map));
                let dist_from_stop = dist_to_maybe_stop_at - current_dist_along;

                // If our lookahead doesn't even hit the intersection / early stopping point, then
                // ignore it. This means we won't request turns until we're close.
                if dist_to_lookahead >= dist_from_stop {
                    let should_stop = if maybe_stop_early.is_some() {
                        true
                    } else if current_router.should_vanish_at_border() {
                        // Don't limit acceleration, but also don't vanish before physically
                        // reaching the border.
                        break;
                    } else {
                        let req =
                            Request::for_car(self.id, current_router.next_step_as_turn().unwrap());
                        let granted = intersections.request_granted(req.clone());
                        if !granted {
                            // Otherwise, we wind up submitting a request at the end of our step, after
                            // we've passed through the intersection!
                            requests.push(req);
                        }
                        !granted
                    };
                    if should_stop {
                        let accel = vehicle.accel_to_stop_in_dist(self.speed, dist_from_stop)?;
                        if self.debug {
                            debug!(
                                "  {} needs {} to stop for something that's currently {} away",
                                self.id, accel, dist_from_stop
                            );
                        }
                        constraints.push(accel);
                        // No use in further lookahead.
                        break;
                    }
                }
            }

            // Advance to the next step.
            let dist_this_step = current_on.length(map) - current_dist_along;
            dist_to_lookahead -= dist_this_step;
            if dist_to_lookahead <= 0.0 * si::M {
                break;
            }
            current_on = current_router.finished_step(current_on).as_traversable();
            current_dist_along = 0.0 * si::M;
            dist_scanned_ahead += dist_this_step;
        }

        // Clamp based on what we can actually do
        // TODO this type mangling is awful
        let safe_accel = vehicle.clamp_accel(
            constraints
                .into_iter()
                .min_by_key(|a| NotNaN::new(a.value_unsafe).unwrap())
                .unwrap(),
        );
        if self.debug {
            let describe_accel = if safe_accel == vehicle.max_accel {
                format!("max_accel ({})", safe_accel)
            } else if safe_accel == vehicle.max_deaccel {
                format!("max_deaccel ({})", safe_accel)
            } else {
                format!("{}", safe_accel)
            };
            let describe_speed = if Some(self.speed) == vehicle.max_speed {
                format!("max_speed ({})", self.speed)
            } else {
                format!("{}", self.speed)
            };
            debug!(
                "At {}, {} chose {}, with current speed {}",
                time, self.id, describe_accel, describe_speed
            );
        }

        Ok(Action::Continue(safe_accel, requests))
    }

    // If true, vanish at the border
    fn step_continue(
        &mut self,
        events: &mut Vec<Event>,
        router: &mut Router,
        accel: Acceleration,
        map: &Map,
        intersections: &mut IntersectionSimState,
    ) -> Result<bool, Error> {
        let (dist, new_speed) = kinematics::results_of_accel_for_one_tick(self.speed, accel);
        self.dist_along += dist;
        self.speed = new_speed;

        loop {
            let current_speed_limit = self.vehicle.clamp_speed(self.on.speed_limit(map));
            if self.speed > current_speed_limit {
                return Err(Error::new(format!(
                    "{} is going {} on {:?}, which has a speed limit of {}",
                    self.id, self.speed, self.on, current_speed_limit
                )));
            }

            let leftover_dist = self.dist_along - self.on.length(map);
            // == 0.0 is important! If no floating point imprecision happens, cars will stop RIGHT
            // at the end of a lane, with exactly 0 leftover distance. We don't want to bump them
            // into the turn and illegally enter the intersection in that case. The alternative
            // from AORTA, IIRC, is to make cars stop anywhere in a small buffer at the end of the
            // lane.
            if leftover_dist <= EPSILON_DIST {
                if leftover_dist > 0.0 * si::M {
                    // But do force them to be right at the end of the Traversable, otherwise we're
                    // in this bizarre, illegal state where dist_along is > the current
                    // Traversable's length.
                    self.dist_along = self.on.length(map) - EPSILON_DIST;
                    // Argh, but don't go negative! Use a different epsilon sometimes?
                    if self.dist_along < 0.0 * si::M {
                        self.dist_along = self.on.length(map) - std::f64::EPSILON * si::M;
                    }
                }
                break;
            }

            if let Traversable::Turn(t) = self.on {
                intersections.on_exit(Request::for_car(self.id, t));
            }
            events.push(Event::AgentLeavesTraversable(
                AgentID::Car(self.id),
                self.on,
            ));

            if router.should_vanish_at_border() {
                return Ok(true);
            }
            match router.finished_step(self.on) {
                PathStep::Lane(id) => {
                    self.on = Traversable::Lane(id);
                }
                PathStep::Turn(id) => {
                    self.on = Traversable::Turn(id);
                    intersections
                        .on_enter(Request::for_car(self.id, id))
                        .map_err(|e| {
                            e.context(format!(
                                "new speed {}, leftover dist {}",
                                self.speed, leftover_dist
                            ))
                        })?;
                }
                x => {
                    return Err(Error::new(format!(
                        "car router had unexpected PathStep {:?}",
                        x
                    )));
                }
            };
            self.dist_along = leftover_dist;
            events.push(Event::AgentEntersTraversable(
                AgentID::Car(self.id),
                self.on,
            ));
        }
        Ok(false)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct SimQueue {
    // First element is farthest along the queue; they have the greatest dist_along.
    // Caching the current dist_along vastly simplifies the API of SimQueue.
    cars_queue: Vec<(Distance, CarID)>,
}

impl SimQueue {
    fn new(
        id: Traversable,
        map: &Map,
        ids: Vec<CarID>,
        cars: &BTreeMap<CarID, Car>,
    ) -> Result<SimQueue, Error> {
        let capacity =
            ((id.length(map) / Vehicle::best_case_following_dist()).ceil() as usize).max(1);
        let mut cars_queue: Vec<(Distance, CarID)> = ids
            .into_iter()
            .map(|id| (cars[&id].dist_along, id))
            .collect();
        if cars_queue.len() > capacity {
            return Err(Error::new(format!(
                "on {:?}, reset to {:?} broke, because capacity is just {}.",
                id, cars_queue, capacity
            )));
        }

        // Sort descending.
        cars_queue.sort_by_key(|(dist, _)| -NotNaN::new(dist.value_unsafe).unwrap());

        // assert here we're not squished together too much
        for slice in cars_queue.windows(2) {
            let ((dist1, c1), (dist2, c2)) = (slice[0], slice[1]);
            let following_dist = cars[&c1].vehicle.following_dist();
            if dist1 - dist2 < following_dist {
                let mut err = format!(
                    "On {:?}, {} and {} are {} apart -- that's {} too close\n",
                    id,
                    c1,
                    c2,
                    dist1 - dist2,
                    following_dist - (dist1 - dist2),
                );
                // TODO We used to have old_queue and could print more debug info. Meh.
                err.push_str(&format!("Queue ({}):\n", cars_queue.len()));
                for (dist, id) in &cars_queue {
                    err.push_str(&format!("- {} at {}\n", id, dist));
                }
                return Err(Error::new(err));
            }
        }
        Ok(SimQueue { cars_queue })
    }

    // TODO it'd be cool to contribute tooltips (like number of cars currently here, capacity) to
    // tooltip

    // TODO this starts cars with their front aligned with the end of the lane, sticking their back
    // into the intersection. :(
    fn get_draw_cars(&self, sim: &DrivingSimState, map: &Map, time: Tick) -> Vec<DrawCarInput> {
        let mut results = Vec::new();
        for (_, id) in &self.cars_queue {
            results.push(sim.get_draw_car(*id, time, map).unwrap())
        }
        results
    }

    // TODO for these three, could use binary search
    pub fn next_car_in_front_of(&self, dist: Distance) -> Option<CarID> {
        self.cars_queue
            .iter()
            .rev()
            .find(|(their_dist, _)| *their_dist > dist)
            .map(|(_, id)| *id)
    }

    fn first_car_behind(&self, dist: Distance) -> Option<CarID> {
        self.cars_queue
            .iter()
            .find(|(their_dist, _)| *their_dist <= dist)
            .map(|(_, id)| *id)
    }

    fn insert_at(&mut self, car: CarID, dist_along: Distance) {
        if let Some(idx) = self
            .cars_queue
            .iter()
            .position(|(their_dist, _)| *their_dist < dist_along)
        {
            self.cars_queue.insert(idx, (dist_along, car));
        } else {
            self.cars_queue.push((dist_along, car));
        }
    }
}

// This manages only actively driving cars
#[derive(Serialize, Deserialize, PartialEq)]
pub struct DrivingSimState {
    // Using BTreeMap instead of HashMap so iteration is deterministic.
    cars: BTreeMap<CarID, Car>,
    // Separate from cars so we can have different mutability in react()
    routers: BTreeMap<CarID, Router>,
    // If there's no SimQueue for a Traversable, then there are currently no agents on it.
    #[serde(
        serialize_with = "serialize_btreemap",
        deserialize_with = "deserialize_btreemap"
    )]
    queues: BTreeMap<Traversable, SimQueue>,
    debug: Option<CarID>,
}

impl DrivingSimState {
    pub fn new() -> DrivingSimState {
        DrivingSimState {
            cars: BTreeMap::new(),
            routers: BTreeMap::new(),
            queues: BTreeMap::new(),
            debug: None,
        }
    }

    pub fn get_active_and_waiting_count(&self) -> (usize, usize) {
        let waiting = self
            .cars
            .values()
            .filter(|c| c.speed <= kinematics::EPSILON_SPEED)
            .count();
        (waiting, self.cars.len())
    }

    pub fn is_done(&self) -> bool {
        self.cars
            .values()
            .filter(|c| c.vehicle.vehicle_type != VehicleType::Bus)
            .count()
            == 0
    }

    pub fn tooltip_lines(&self, id: CarID) -> Option<Vec<String>> {
        if let Some(c) = self.cars.get(&id) {
            Some(vec![
                format!("Car {:?}, part of {:?}, owned by {:?}", id, c.trip, c.owner),
                format!(
                    "On {:?}, speed {:?}, dist along {:?}",
                    c.on, c.speed, c.dist_along
                ),
                self.routers[&id].tooltip_line(),
            ])
        } else {
            None
        }
    }

    pub fn toggle_debug(&mut self, id: CarID) {
        if let Some(c) = self.debug {
            if c != id {
                let car = self.cars.get_mut(&c).unwrap();
                car.debug = false;
                car.vehicle.debug = false;
            }
        }

        if let Some(car) = self.cars.get_mut(&id) {
            println!("{}", abstutil::to_json(car));
            println!("{}", abstutil::to_json(&self.routers[&id]));
            car.debug = !car.debug;
            car.vehicle.debug = !car.vehicle.debug;
            self.debug = Some(id);
        } else {
            println!("{} is parked somewhere", id);
        }
    }

    pub fn edit_remove_lane(&mut self, id: LaneID) {
        assert!(!self.queues.contains_key(&Traversable::Lane(id)));
    }

    pub fn edit_add_lane(&mut self, _id: LaneID) {}

    pub fn edit_remove_turn(&mut self, id: TurnID) {
        assert!(!self.queues.contains_key(&Traversable::Turn(id)));
    }

    pub fn edit_add_turn(&mut self, _id: TurnID) {}

    // Note that this populates the view BEFORE the step is applied.
    // Returns
    // 1) cars that reached a parking spot this step
    // 2) the cars that vanished at a border
    // 3) the bikes that reached some ending and should start parking
    pub fn step(
        &mut self,
        view: &mut WorldView,
        events: &mut Vec<Event>,
        time: Tick,
        map: &Map,
        // TODO not all of it, just for one query!
        parking_sim: &ParkingSimState,
        intersections: &mut IntersectionSimState,
        transit_sim: &mut TransitSimState,
        rng: &mut XorShiftRng,
        current_agent: &mut Option<AgentID>,
    ) -> Result<(Vec<ParkedCar>, Vec<CarID>, Vec<(CarID, Position)>), Error> {
        // We don't need the queues at all during this function, so just move them to the view.
        std::mem::swap(&mut view.queues, &mut self.queues);
        self.populate_view(view);

        // Could be concurrent, since this is deterministic -- EXCEPT for the rng, used to
        // sometimes pick a next lane to try for parking.
        let mut requested_moves: Vec<(CarID, Action)> = Vec::new();
        for c in self.cars.values() {
            *current_agent = Some(AgentID::Car(c.id));
            requested_moves.push((
                c.id,
                c.react(
                    events,
                    self.routers.get_mut(&c.id).unwrap(),
                    rng,
                    map,
                    time,
                    &view,
                    parking_sim,
                    transit_sim,
                    intersections,
                )?,
            ));
        }

        // In AORTA, there was a split here -- react vs step phase. We're still following the same
        // thing, but it might be slightly more clear to express it differently?

        let mut finished_parking: Vec<ParkedCar> = Vec::new();
        let mut vanished_at_border: Vec<CarID> = Vec::new();
        // The lane is the where the bike ended, so NOT a sidewalk
        let mut done_biking: Vec<(CarID, Position)> = Vec::new();

        // Apply moves. Since lookahead behavior works, there are no conflicts to resolve, meaning
        // this could be applied concurrently!
        for (id, act) in &requested_moves {
            *current_agent = Some(AgentID::Car(*id));
            match *act {
                Action::StartParking(ref spot) => {
                    let c = self.cars.get_mut(&id).unwrap();
                    c.parking = Some(ParkingState {
                        is_parking: true,
                        started_at: time,
                        tuple: ParkedCar::new(*id, *spot, c.vehicle.clone(), c.owner),
                    });
                }
                Action::WorkOnParking => {
                    let state = self.cars.get_mut(&id).unwrap().parking.take().unwrap();
                    if state.started_at + TIME_TO_PARK_OR_DEPART == time {
                        if state.is_parking {
                            finished_parking.push(state.tuple);
                            // No longer need to represent the car in the driving state
                            self.cars.remove(&id);
                            self.routers.remove(&id);
                        }
                    } else {
                        self.cars.get_mut(&id).unwrap().parking = Some(state);
                    }
                }
                Action::StartParkingBike => {
                    {
                        let c = self.cars.get(&id).unwrap();
                        done_biking.push((*id, Position::new(c.on.as_lane(), c.dist_along)));
                    }
                    self.cars.remove(&id);
                    self.routers.remove(&id);
                }
                Action::Continue(accel, ref requests) => {
                    let done = {
                        let c = self.cars.get_mut(&id).unwrap();
                        c.step_continue(
                            events,
                            self.routers.get_mut(&id).unwrap(),
                            accel,
                            map,
                            intersections,
                        )?
                    };
                    if done {
                        self.cars.remove(&id);
                        self.routers.remove(&id);
                        vanished_at_border.push(*id);
                    } else {
                        // TODO maybe just return TurnID
                        for req in requests {
                            // Note this is idempotent and does NOT grant the request.
                            // TODO should we check that the car is currently the lead vehicle?
                            // intersection is assuming that! or relax that assumption.
                            intersections.submit_request(req.clone());
                        }
                    }
                }
                Action::VanishAtDeadEnd => {
                    self.cars.remove(&id);
                    self.routers.remove(&id);
                }
            }
        }
        *current_agent = None;

        // Group cars by lane and turn
        let mut cars_per_traversable = MultiMap::new();
        for c in self.cars.values() {
            // Also do some sanity checks.
            if c.dist_along < 0.0 * si::M {
                return Err(Error::new(format!(
                    "{} is {} along {:?}",
                    c.id, c.dist_along, c.on
                )));
            }
            cars_per_traversable.insert(c.on, c.id);
        }

        // Reset all queues -- only store ones with some agents present.
        for (on, cars) in cars_per_traversable.into_iter() {
            self.queues
                .insert(on, SimQueue::new(on, map, cars, &self.cars)?);
        }

        Ok((finished_parking, vanished_at_border, done_biking))
    }

    // True if the car started, false if there wasn't currently room
    pub fn start_car_on_lane(
        &mut self,
        events: &mut Vec<Event>,
        time: Tick,
        map: &Map,
        params: CreateCar,
    ) -> bool {
        let start_lane = params.start.lane();
        let start_on = Traversable::Lane(start_lane);
        let start_dist = params.start.dist_along();

        // The caller should have passed in a Position for the driving lane. But sanity check!
        {
            let start_length = map.get_l(start_lane).length();
            if start_dist > start_length {
                panic!(
                    "Can't start car at {} along {}; it's only {}. Bad position passed in.",
                    start_dist, start_lane, start_length
                );
            }
        }

        // Is it safe to enter the lane right now? Start scanning ahead of where we'll enter, so we
        // don't hit somebody's back
        if let Some(other) = self
            .queues
            .get(&start_on)
            .and_then(|q| q.first_car_behind(start_dist + Vehicle::worst_case_following_dist()))
        {
            let other_dist = self.cars[&other].dist_along;
            if other_dist >= start_dist {
                /*debug!(
                    "{} can't spawn, because they'd wind up too close ({}) behind {}",
                    params.car,
                    other_dist - params.dist_along,
                    other
                );*/
                return false;
            }

            let other_vehicle = &self.cars[&other].vehicle;
            let accel_for_other_to_stop = other_vehicle
                .accel_to_follow(
                    self.cars[&other].speed,
                    other_vehicle.clamp_speed(map.get_parent(start_lane).get_speed_limit()),
                    &params.vehicle,
                    start_dist - other_dist,
                    0.0 * si::MPS,
                ).unwrap();
            if accel_for_other_to_stop <= other_vehicle.max_deaccel {
                //debug!("{} can't spawn {} in front of {}, because {} would have to do {} to not hit {}", params.car, params.dist_along - other_dist, other, other, accel_for_other_to_stop, params.car);
                return false;
            }

            // TODO check that there's not a car elsewhere that's about to wind up here. can check
            // the intersection for accepted turns to this lane. or, enforce that no parking spots
            // can exist before the worst-case entry distance (based on the speed limit).
        }

        self.cars.insert(
            params.car,
            Car {
                id: params.car,
                trip: params.trip,
                owner: params.owner,
                on: start_on,
                dist_along: start_dist,
                speed: 0.0 * si::MPS,
                vehicle: params.vehicle,
                debug: false,
                parking: params.maybe_parked_car.and_then(|parked_car| {
                    Some(ParkingState {
                        is_parking: false,
                        started_at: time,
                        tuple: parked_car,
                    })
                }),
            },
        );
        self.routers.insert(params.car, params.router);
        // TODO stupid lack of NLL
        let queue_exists = self.queues.contains_key(&start_on);
        if queue_exists {
            self.queues
                .get_mut(&start_on)
                .unwrap()
                .insert_at(params.car, start_dist);
        } else {
            // Just directly construct the SimQueue, since it only has one car
            self.queues.insert(
                start_on,
                SimQueue {
                    cars_queue: vec![(start_dist, params.car)],
                },
            );
        }

        events.push(Event::AgentEntersTraversable(
            AgentID::Car(params.car),
            start_on,
        ));
        true
    }

    pub fn get_draw_car(&self, id: CarID, time: Tick, map: &Map) -> Option<DrawCarInput> {
        let c = self.cars.get(&id)?;
        let (base_pos, angle) = c.on.dist_along(c.dist_along, map);

        // TODO arguably, this math might belong in DrawCar.
        let pos = if let Some(ref parking) = c.parking {
            let progress: f64 =
                ((time - parking.started_at).as_time() / TIME_TO_PARK_OR_DEPART).value_unsafe;
            assert!(progress >= 0.0 && progress <= 1.0);
            let project_away_ratio = if parking.is_parking {
                progress
            } else {
                1.0 - progress
            };
            // TODO we're assuming the parking lane is to the right of us!
            base_pos.project_away(project_away_ratio * LANE_THICKNESS, angle.rotate_degs(90.0))
        } else {
            base_pos
        };

        Some(DrawCarInput {
            id: c.id,
            vehicle_length: c.vehicle.length,
            waiting_for_turn: self.routers[&c.id].next_step_as_turn(),
            front: pos,
            angle,
            stopping_trace: self.trace_route(
                id,
                map,
                c.vehicle.stopping_distance(c.speed).unwrap(),
            ),
            state: if c.debug {
                CarState::Debug
            } else if c.speed > kinematics::EPSILON_SPEED {
                CarState::Moving
            } else {
                CarState::Stuck
            },
            vehicle_type: c.vehicle.vehicle_type,
        })
    }

    pub fn get_draw_cars(&self, on: Traversable, time: Tick, map: &Map) -> Vec<DrawCarInput> {
        if let Some(queue) = self.queues.get(&on) {
            return queue.get_draw_cars(self, map, time);
        }
        return Vec::new();
    }

    fn populate_view(&mut self, view: &mut WorldView) {
        for c in self.cars.values() {
            view.agents.insert(
                AgentID::Car(c.id),
                AgentView {
                    id: AgentID::Car(c.id),
                    debug: c.debug,
                    on: c.on,
                    dist_along: c.dist_along,
                    speed: c.speed,
                    vehicle: Some(c.vehicle.clone()),
                },
            );
        }
    }

    pub fn trace_route(&self, id: CarID, map: &Map, dist_ahead: Distance) -> Option<Trace> {
        if dist_ahead <= EPSILON_DIST {
            return None;
        }
        let r = self.routers.get(&id)?;
        let c = &self.cars[&id];
        r.trace_route(c.dist_along, map, dist_ahead)
    }

    pub fn get_path(&self, id: CarID) -> Option<&Path> {
        let r = self.routers.get(&id)?;
        Some(r.get_path())
    }

    pub fn get_owner_of_car(&self, id: CarID) -> Option<BuildingID> {
        let c = &self.cars.get(&id)?;
        c.owner
    }

    // TODO turns too
    pub fn count(&self, lanes: &HashSet<LaneID>) -> (usize, usize, usize) {
        let mut moving_cars = 0;
        let mut stuck_cars = 0;
        let mut buses = 0;

        for l in lanes {
            if let Some(queue) = self.queues.get(&Traversable::Lane(*l)) {
                for (_, car) in &queue.cars_queue {
                    let c = &self.cars[car];
                    if c.speed <= kinematics::EPSILON_SPEED {
                        stuck_cars += 1;
                    } else {
                        moving_cars += 1;
                    }
                    if c.vehicle.vehicle_type == VehicleType::Bus {
                        buses += 1;
                    }
                }
            }
        }

        (moving_cars, stuck_cars, buses)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct CreateCar {
    pub car: CarID,
    pub trip: TripID,
    pub owner: Option<BuildingID>,
    pub maybe_parked_car: Option<ParkedCar>,
    pub vehicle: Vehicle,
    pub start: Position,
    pub router: Router,
}
