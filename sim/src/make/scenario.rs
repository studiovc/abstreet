use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fmt;

use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use serde::{Deserialize, Serialize};

use abstutil::{prettyprint_usize, Counter, MapName, Parallelism, Timer};
use geom::{Distance, Speed, Time};
use map_model::{
    BuildingID, BusRouteID, BusStopID, DirectedRoadID, Map, OffstreetParking, PathConstraints,
    Position, RoadID,
};

use crate::make::fork_rng;
use crate::{
    CarID, DrivingGoal, OrigPersonID, ParkingSpot, PersonID, SidewalkSpot, Sim, TripEndpoint,
    TripInfo, TripMode, TripSpawner, TripSpec, Vehicle, VehicleSpec, VehicleType, BIKE_LENGTH,
    MAX_CAR_LENGTH, MIN_CAR_LENGTH, SPAWN_DIST,
};

/// A Scenario describes all the input to a simulation. Usually a scenario covers one day.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Scenario {
    pub scenario_name: String,
    pub map_name: MapName,

    pub people: Vec<PersonSpec>,
    /// None means seed all buses. Otherwise the route name must be present here.
    pub only_seed_buses: Option<BTreeSet<String>>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PersonSpec {
    pub id: PersonID,
    /// Just used for debugging
    pub orig_id: Option<OrigPersonID>,
    pub trips: Vec<IndividTrip>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct IndividTrip {
    pub depart: Time,
    pub from: TripEndpoint,
    pub to: TripEndpoint,
    pub mode: TripMode,
    pub purpose: TripPurpose,
    pub cancelled: bool,
    /// Did a ScenarioModifier affect this?
    pub modified: bool,
}

impl IndividTrip {
    pub fn new(
        depart: Time,
        purpose: TripPurpose,
        from: TripEndpoint,
        to: TripEndpoint,
        mode: TripMode,
    ) -> IndividTrip {
        IndividTrip {
            depart,
            from,
            to,
            mode,
            purpose,
            cancelled: false,
            modified: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum SpawnTrip {
    /// Only for interactive / debug trips
    VehicleAppearing {
        start: Position,
        goal: DrivingGoal,
        is_bike: bool,
    },
    FromBorder {
        dr: DirectedRoadID,
        goal: DrivingGoal,
        /// For bikes starting at a border, use FromBorder. UsingBike implies a walk->bike trip.
        is_bike: bool,
    },
    UsingParkedCar(BuildingID, DrivingGoal),
    UsingBike(BuildingID, DrivingGoal),
    JustWalking(SidewalkSpot, SidewalkSpot),
    UsingTransit(
        SidewalkSpot,
        SidewalkSpot,
        BusRouteID,
        BusStopID,
        Option<BusStopID>,
    ),
}

/// Lifted from Seattle's Soundcast model, but seems general enough to use anyhere.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum TripPurpose {
    Home,
    Work,
    School,
    Escort,
    PersonalBusiness,
    Shopping,
    Meal,
    Social,
    Recreation,
    Medical,
    ParkAndRideTransfer,
}

impl fmt::Display for TripPurpose {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                TripPurpose::Home => "home",
                TripPurpose::Work => "work",
                TripPurpose::School => "school",
                // Is this like a parent escorting a child to school?
                TripPurpose::Escort => "escort",
                TripPurpose::PersonalBusiness => "personal business",
                TripPurpose::Shopping => "shopping",
                TripPurpose::Meal => "eating",
                TripPurpose::Social => "social",
                TripPurpose::Recreation => "recreation",
                TripPurpose::Medical => "medical",
                TripPurpose::ParkAndRideTransfer => "park-and-ride transfer",
            }
        )
    }
}

impl Scenario {
    pub fn instantiate(&self, sim: &mut Sim, map: &Map, rng: &mut XorShiftRng, timer: &mut Timer) {
        self.instantiate_without_retries(sim, map, rng, true, timer);
    }

    /// If retry_if_no_room is false, any vehicles that fail to spawn because of something else in
    /// the way will just wind up as cancelled trips.
    pub fn instantiate_without_retries(
        &self,
        sim: &mut Sim,
        map: &Map,
        rng: &mut XorShiftRng,
        retry_if_no_room: bool,
        timer: &mut Timer,
    ) {
        // Any case where map edits could change the calls to the RNG, we have to fork.
        sim.set_name(self.scenario_name.clone());

        timer.start(format!("Instantiating {}", self.scenario_name));

        if let Some(ref routes) = self.only_seed_buses {
            for route in map.all_bus_routes() {
                if routes.contains(&route.full_name) {
                    sim.seed_bus_route(route);
                }
            }
        } else {
            // All of them
            for route in map.all_bus_routes() {
                sim.seed_bus_route(route);
            }
        }

        timer.start_iter("trips for People", self.people.len());
        let mut parked_cars: Vec<(Vehicle, BuildingID)> = Vec::new();
        let mut schedule_trips = Vec::new();
        for p in &self.people {
            timer.next();

            if let Err(err) = p.check_schedule() {
                panic!("{}", err);
            }

            let (vehicle_specs, cars_initially_parked_at, vehicle_foreach_trip) =
                p.get_vehicles(rng);
            sim.new_person(
                p.id,
                p.orig_id,
                Scenario::rand_ped_speed(rng),
                vehicle_specs,
            );
            let person = sim.get_person(p.id);
            for (idx, b) in cars_initially_parked_at {
                parked_cars.push((person.vehicles[idx].clone(), b));
            }
            for (t, maybe_idx) in p.trips.iter().zip(vehicle_foreach_trip) {
                // The RNG call might change over edits for picking the spawning lane from a border
                // with multiple choices for a vehicle type.
                let mut tmp_rng = fork_rng(rng);
                let spec = match SpawnTrip::new(t.from.clone(), t.to.clone(), t.mode, map) {
                    Some(trip) => trip.to_trip_spec(
                        maybe_idx.map(|idx| person.vehicles[idx].id),
                        retry_if_no_room,
                        &mut tmp_rng,
                        map,
                    ),
                    None => TripSpec::SpawningFailure {
                        use_vehicle: maybe_idx.map(|idx| person.vehicles[idx].id),
                        // TODO Collapse SpawnTrip::new and to_trip_spec and plumb better errors
                        error: format!("unknown spawning error"),
                    },
                };
                schedule_trips.push((
                    person.id,
                    spec,
                    TripInfo {
                        departure: t.depart,
                        mode: t.mode,
                        start: t.from.clone(),
                        end: t.to.clone(),
                        purpose: t.purpose,
                        modified: t.modified,
                        capped: false,
                        cancellation_reason: if t.cancelled {
                            Some(format!("cancelled by ScenarioModifier"))
                        } else {
                            None
                        },
                    },
                ));
            }
        }

        let mut spawner = TripSpawner::new();
        let results = timer.parallelize(
            "schedule trips",
            Parallelism::Fastest,
            schedule_trips,
            |tuple| spawner.schedule_trip(tuple.0, tuple.1, tuple.2, map),
        );
        spawner.schedule_trips(results);

        // parked_cars is stable over map edits, so don't fork.
        parked_cars.shuffle(rng);
        seed_parked_cars(parked_cars, sim, map, rng, timer);

        sim.flush_spawner(spawner, map, timer);
        timer.stop(format!("Instantiating {}", self.scenario_name));
    }

    pub fn save(&self) {
        abstutil::write_binary(
            abstutil::path_scenario(&self.map_name, &self.scenario_name),
            self,
        );
    }

    pub fn empty(map: &Map, name: &str) -> Scenario {
        Scenario {
            scenario_name: name.to_string(),
            map_name: map.get_name().clone(),
            people: Vec::new(),
            only_seed_buses: Some(BTreeSet::new()),
        }
    }

    fn rand_car(rng: &mut XorShiftRng) -> VehicleSpec {
        let length = Scenario::rand_dist(rng, MIN_CAR_LENGTH, MAX_CAR_LENGTH);
        VehicleSpec {
            vehicle_type: VehicleType::Car,
            length,
            max_speed: None,
        }
    }

    fn rand_bike(rng: &mut XorShiftRng) -> VehicleSpec {
        let max_speed = Some(Scenario::rand_speed(
            rng,
            Speed::miles_per_hour(8.0),
            Scenario::max_bike_speed(),
        ));
        VehicleSpec {
            vehicle_type: VehicleType::Bike,
            length: BIKE_LENGTH,
            max_speed,
        }
    }
    pub fn max_bike_speed() -> Speed {
        Speed::miles_per_hour(10.0)
    }

    pub fn rand_dist(rng: &mut XorShiftRng, low: Distance, high: Distance) -> Distance {
        assert!(high > low);
        Distance::meters(rng.gen_range(low.inner_meters(), high.inner_meters()))
    }

    fn rand_speed(rng: &mut XorShiftRng, low: Speed, high: Speed) -> Speed {
        assert!(high > low);
        Speed::meters_per_second(rng.gen_range(
            low.inner_meters_per_second(),
            high.inner_meters_per_second(),
        ))
    }

    pub fn rand_ped_speed(rng: &mut XorShiftRng) -> Speed {
        Scenario::rand_speed(rng, Speed::miles_per_hour(2.0), Speed::miles_per_hour(3.0))
    }
    pub fn max_ped_speed() -> Speed {
        Speed::miles_per_hour(3.0)
    }

    pub fn count_parked_cars_per_bldg(&self) -> Counter<BuildingID> {
        let mut per_bldg = Counter::new();
        // Pass in a dummy RNG
        let mut rng = XorShiftRng::seed_from_u64(0);
        for p in &self.people {
            let (_, cars_initially_parked_at, _) = p.get_vehicles(&mut rng);
            for (_, b) in cars_initially_parked_at {
                per_bldg.inc(b);
            }
        }
        per_bldg
    }

    pub fn remove_weird_schedules(mut self) -> Scenario {
        let orig = self.people.len();
        self.people.retain(|person| match person.check_schedule() {
            Ok(()) => true,
            Err(err) => {
                println!("{}", err);
                false
            }
        });
        println!(
            "{} of {} people have nonsense schedules",
            prettyprint_usize(orig - self.people.len()),
            prettyprint_usize(orig)
        );
        // Fix up IDs
        for (idx, person) in self.people.iter_mut().enumerate() {
            person.id = PersonID(idx);
        }
        self
    }
}

fn seed_parked_cars(
    parked_cars: Vec<(Vehicle, BuildingID)>,
    sim: &mut Sim,
    map: &Map,
    base_rng: &mut XorShiftRng,
    timer: &mut Timer,
) {
    if sim.infinite_parking() {
        let mut blackholed = 0;
        for (vehicle, b) in parked_cars {
            if let Some(spot) = sim.get_free_offstreet_spots(b).pop() {
                sim.seed_parked_car(vehicle, spot);
            } else {
                blackholed += 1;
            }
        }
        if blackholed > 0 {
            timer.warn(format!(
                "{} parked cars weren't seeded, due to blackholed buildings",
                prettyprint_usize(blackholed)
            ));
        }
        return;
    }

    let mut open_spots_per_road: BTreeMap<RoadID, Vec<(ParkingSpot, Option<BuildingID>)>> =
        BTreeMap::new();
    for spot in sim.get_all_parking_spots().1 {
        let (r, restriction) = match spot {
            ParkingSpot::Onstreet(l, _) => (map.get_l(l).parent, None),
            ParkingSpot::Offstreet(b, _) => (
                map.get_l(map.get_b(b).sidewalk()).parent,
                match map.get_b(b).parking {
                    OffstreetParking::PublicGarage(_, _) => None,
                    OffstreetParking::Private(_, _) => Some(b),
                },
            ),
            ParkingSpot::Lot(pl, _) => (map.get_l(map.get_pl(pl).driving_pos.lane()).parent, None),
        };
        open_spots_per_road
            .entry(r)
            .or_insert_with(Vec::new)
            .push((spot, restriction));
    }
    // Changing parking on one road shouldn't affect far-off roads. Fork carefully.
    for r in map.all_roads() {
        let mut tmp_rng = fork_rng(base_rng);
        if let Some(ref mut spots) = open_spots_per_road.get_mut(&r.id) {
            spots.shuffle(&mut tmp_rng);
        }
    }

    timer.start_iter("seed parked cars", parked_cars.len());
    let mut ok = true;
    let total_cars = parked_cars.len();
    let mut seeded = 0;
    for (vehicle, b) in parked_cars {
        timer.next();
        if !ok {
            continue;
        }
        if let Some(spot) = find_spot_near_building(b, &mut open_spots_per_road, map) {
            seeded += 1;
            sim.seed_parked_car(vehicle, spot);
        } else {
            timer.warn(format!(
                "Not enough room to seed parked cars. Only found spots for {} of {}",
                prettyprint_usize(seeded),
                prettyprint_usize(total_cars)
            ));
            ok = false;
        }
    }
}

// Pick a parking spot for this building. If the building's road has a free spot, use it. If not,
// start BFSing out from the road in a deterministic way until finding a nearby road with an open
// spot.
fn find_spot_near_building(
    b: BuildingID,
    open_spots_per_road: &mut BTreeMap<RoadID, Vec<(ParkingSpot, Option<BuildingID>)>>,
    map: &Map,
) -> Option<ParkingSpot> {
    let mut roads_queue: VecDeque<RoadID> = VecDeque::new();
    let mut visited: HashSet<RoadID> = HashSet::new();
    {
        let start = map.building_to_road(b).id;
        roads_queue.push_back(start);
        visited.insert(start);
    }

    loop {
        let r = roads_queue.pop_front()?;
        if let Some(spots) = open_spots_per_road.get_mut(&r) {
            // Fill in all private parking first before
            // TODO With some probability, skip this available spot and park farther away
            if let Some(idx) = spots
                .iter()
                .position(|(_, restriction)| restriction == &Some(b))
            {
                return Some(spots.remove(idx).0);
            }
            if let Some(idx) = spots
                .iter()
                .position(|(_, restriction)| restriction.is_none())
            {
                return Some(spots.remove(idx).0);
            }
        }

        for next_r in map.get_next_roads(r).into_iter() {
            if !visited.contains(&next_r) {
                roads_queue.push_back(next_r);
                visited.insert(next_r);
            }
        }
    }
}

impl SpawnTrip {
    fn to_trip_spec(
        self,
        use_vehicle: Option<CarID>,
        retry_if_no_room: bool,
        rng: &mut XorShiftRng,
        map: &Map,
    ) -> TripSpec {
        match self {
            SpawnTrip::VehicleAppearing { start, goal, .. } => TripSpec::VehicleAppearing {
                start_pos: start,
                goal,
                use_vehicle: use_vehicle.unwrap(),
                retry_if_no_room,
            },
            SpawnTrip::FromBorder { dr, goal, is_bike } => {
                let constraints = if is_bike {
                    PathConstraints::Bike
                } else {
                    PathConstraints::Car
                };
                if let Some(l) = dr.lanes(constraints, map).choose(rng) {
                    TripSpec::VehicleAppearing {
                        start_pos: Position::new(*l, SPAWN_DIST),
                        goal,
                        use_vehicle: use_vehicle.unwrap(),
                        retry_if_no_room,
                    }
                } else {
                    TripSpec::SpawningFailure {
                        use_vehicle,
                        error: format!("{} has no lanes to spawn a {:?}", dr.id, constraints),
                    }
                }
            }
            SpawnTrip::UsingParkedCar(start_bldg, goal) => TripSpec::UsingParkedCar {
                start_bldg,
                goal,
                car: use_vehicle.unwrap(),
            },
            SpawnTrip::UsingBike(start, goal) => TripSpec::UsingBike {
                bike: use_vehicle.unwrap(),
                start,
                goal,
            },
            SpawnTrip::JustWalking(start, goal) => TripSpec::JustWalking { start, goal },
            SpawnTrip::UsingTransit(start, goal, route, stop1, maybe_stop2) => {
                TripSpec::UsingTransit {
                    start,
                    goal,
                    route,
                    stop1,
                    maybe_stop2,
                }
            }
        }
    }

    fn new(from: TripEndpoint, to: TripEndpoint, mode: TripMode, map: &Map) -> Option<SpawnTrip> {
        Some(match mode {
            TripMode::Drive => match from {
                TripEndpoint::Bldg(b) => {
                    SpawnTrip::UsingParkedCar(b, to.driving_goal(PathConstraints::Car, map)?)
                }
                TripEndpoint::Border(i) => SpawnTrip::FromBorder {
                    dr: map.get_i(i).some_outgoing_road(map)?,
                    goal: to.driving_goal(PathConstraints::Car, map)?,
                    is_bike: false,
                },
                TripEndpoint::SuddenlyAppear(start) => SpawnTrip::VehicleAppearing {
                    start,
                    goal: to.driving_goal(PathConstraints::Bike, map)?,
                    is_bike: false,
                },
            },
            TripMode::Bike => match from {
                TripEndpoint::Bldg(b) => {
                    SpawnTrip::UsingBike(b, to.driving_goal(PathConstraints::Bike, map)?)
                }
                TripEndpoint::Border(i) => SpawnTrip::FromBorder {
                    dr: map.get_i(i).some_outgoing_road(map)?,
                    goal: to.driving_goal(PathConstraints::Bike, map)?,
                    is_bike: true,
                },
                TripEndpoint::SuddenlyAppear(start) => SpawnTrip::VehicleAppearing {
                    start,
                    goal: to.driving_goal(PathConstraints::Bike, map)?,
                    is_bike: true,
                },
            },
            TripMode::Walk => {
                SpawnTrip::JustWalking(from.start_sidewalk_spot(map)?, to.end_sidewalk_spot(map)?)
            }
            TripMode::Transit => {
                let start = from.start_sidewalk_spot(map)?;
                let goal = to.end_sidewalk_spot(map)?;
                if let Some((stop1, maybe_stop2, route)) =
                    map.should_use_transit(start.sidewalk_pos, goal.sidewalk_pos)
                {
                    SpawnTrip::UsingTransit(start, goal, route, stop1, maybe_stop2)
                } else {
                    //timer.warn(format!("{:?} not actually using transit, because pathfinding
                    // didn't find any useful route", trip));
                    SpawnTrip::JustWalking(start, goal)
                }
            }
        })
    }
}

impl PersonSpec {
    // Verify that the trip start/endpoints of the person match up
    fn check_schedule(&self) -> Result<(), String> {
        for pair in self.trips.iter().zip(self.trips.iter().skip(1)) {
            if pair.0.depart >= pair.1.depart {
                return Err(format!(
                    "{} {:?} starts two trips in the wrong order: {} then {}",
                    self.id, self.orig_id, pair.0.depart, pair.1.depart
                ));
            }

            // Once off-map, re-enter via any border node.
            let end_bldg = match pair.0.to {
                TripEndpoint::Bldg(b) => Some(b),
                TripEndpoint::Border(_) | TripEndpoint::SuddenlyAppear(_) => None,
            };
            let start_bldg = match pair.1.from {
                TripEndpoint::Bldg(b) => Some(b),
                TripEndpoint::Border(_) | TripEndpoint::SuddenlyAppear(_) => None,
            };

            if end_bldg != start_bldg {
                return Err(format!(
                    "At {}, {} {:?} warps between some trips, from {:?} to {:?}",
                    pair.1.depart, self.id, self.orig_id, end_bldg, start_bldg
                ));
            }
        }
        Ok(())
    }

    fn get_vehicles(
        &self,
        rng: &mut XorShiftRng,
    ) -> (
        Vec<VehicleSpec>,
        Vec<(usize, BuildingID)>,
        Vec<Option<usize>>,
    ) {
        let mut vehicle_specs = Vec::new();
        let mut cars_initially_parked_at = Vec::new();
        let mut vehicle_foreach_trip = Vec::new();

        let mut bike_idx = None;
        // For each indexed car, is it parked somewhere, or off-map?
        let mut car_locations: Vec<(usize, Option<BuildingID>)> = Vec::new();

        // TODO If the trip is cancelled, this should be affected...
        for trip in &self.trips {
            let use_for_trip = match trip.mode {
                TripMode::Walk | TripMode::Transit => None,
                TripMode::Bike => {
                    if bike_idx.is_none() {
                        bike_idx = Some(vehicle_specs.len());
                        vehicle_specs.push(Scenario::rand_bike(rng));
                    }
                    bike_idx
                }
                TripMode::Drive => {
                    let need_parked_at = match trip.from {
                        TripEndpoint::Bldg(b) => Some(b),
                        _ => None,
                    };

                    // Any available cars in the right spot?
                    let idx = if let Some(idx) = car_locations
                        .iter()
                        .find(|(_, parked_at)| *parked_at == need_parked_at)
                        .map(|(idx, _)| *idx)
                    {
                        idx
                    } else {
                        // Need a new car, starting in the right spot
                        let idx = vehicle_specs.len();
                        vehicle_specs.push(Scenario::rand_car(rng));
                        if let Some(b) = need_parked_at {
                            cars_initially_parked_at.push((idx, b));
                        }
                        idx
                    };

                    // Where does this car wind up?
                    car_locations.retain(|(i, _)| idx != *i);
                    match trip.to {
                        TripEndpoint::Bldg(b) => {
                            car_locations.push((idx, Some(b)));
                        }
                        TripEndpoint::Border(_) | TripEndpoint::SuddenlyAppear(_) => {
                            car_locations.push((idx, None));
                        }
                    }

                    Some(idx)
                }
            };
            vehicle_foreach_trip.push(use_for_trip);
        }

        // For debugging
        if false {
            let mut n = vehicle_specs.len();
            if bike_idx.is_some() {
                n -= 1;
            }
            if n > 1 {
                println!("{} needs {} cars", self.id, n);
            }
        }

        (
            vehicle_specs,
            cars_initially_parked_at,
            vehicle_foreach_trip,
        )
    }
}