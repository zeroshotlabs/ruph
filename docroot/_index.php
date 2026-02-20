<?php

// Basic per-request init
header("Content-Type: text/plain");

foreach( [1,2,3] as $v )
    echo "ruph ok $v\n";

echo "path: " . $_SERVER['REQUEST_URI'] . "\n";


$name = 'hans';

echo $name;
echo var_dump($name);
var_dump(10);
?>

<h2>
<?=$name?>
</h2>



  <?php
  var_dump(1);
  echo "after\n";
  echo var_dump('2');
  ?>

